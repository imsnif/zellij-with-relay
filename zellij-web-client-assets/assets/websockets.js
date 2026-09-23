import { handleReconnection, handleDisconnected, markConnectionEstablished } from "/assets/connection.js";
import {
    getBaseUrl,
    getWsApiBase,
    isRelayMode,
    isCurrentLocation,
} from "/assets/utils.js";
import { setSoftKeyboard } from "./input.js";
import { applyTerminalBackground, terminalConfigOptions } from "./terminal.js";
import {
    encrypt,
    decrypt,
    encryptSeq,
    decryptSeq,
    FRAME_TYPE_TERMINAL,
    FRAME_TYPE_CONTROL,
    DIRECTION_SHARER_TO_VIEWER,
    DIRECTION_VIEWER_TO_SHARER,
} from "/assets/crypto.js";
import { createClipper } from "./clip.js";

let lastSentCellDimensions = null;

const READ_ONLY_ALLOWED_CONTROL = new Set(["SoftKeyboardVisibilityChanged"]);

function getCellPixelDimensions(term) {
    try {
        const cell =
            term && term._core && term._core._renderService &&
            term._core._renderService.dimensions &&
            term._core._renderService.dimensions.css &&
            term._core._renderService.dimensions.css.cell;
        if (cell && cell.width && cell.height) {
            return { width: cell.width, height: cell.height };
        }
    } catch (_) {}
    const el = term && term.element &&
        term.element.querySelector(".xterm-char-measure-element");
    if (el) {
        const rect = el.getBoundingClientRect();
        if (rect.width && rect.height) {
            return { width: rect.width, height: rect.height };
        }
    }
    return null;
}

function getMobileRenderSizing() {
    return window.__zjMobileUi && window.__zjMobileUi.getRenderSizing
        ? window.__zjMobileUi.getRenderSizing()
        : { pinned: false };
}

function sendSizeUpdate(controlSend, ownWebClientId, term, rows, cols) {
    if (!controlSend || !ownWebClientId) {
        return;
    }
    controlSend({
        web_client_id: ownWebClientId,
        payload: {
            type: "TerminalResize",
            rows,
            cols,
        },
    });
    const cell = getCellPixelDimensions(term);
    if (!cell) {
        return;
    }
    const cellWidth = Math.round(cell.width);
    const cellHeight = Math.round(cell.height);
    if (
        lastSentCellDimensions &&
        lastSentCellDimensions.width === cellWidth &&
        lastSentCellDimensions.height === cellHeight
    ) {
        return;
    }
    lastSentCellDimensions = { width: cellWidth, height: cellHeight };
    controlSend({
        web_client_id: ownWebClientId,
        payload: {
            type: "TerminalMetrics",
            cell_pixel_width: cellWidth,
            cell_pixel_height: cellHeight,
            text_area_pixel_width: Math.round(cols * cell.width),
            text_area_pixel_height: Math.round(rows * cell.height),
        },
    });
}

/**
 * Initialize both terminal and control WebSocket connections
 * @param {string} webClientId - Client ID from authentication
 * @param {string} sessionName - Session name from URL
 * @param {Terminal} term - Terminal instance
 * @param {FitAddon} fitAddon - Terminal fit addon
 * @param {function} sendAnsiKey - Function to send ANSI key sequences
 * @param {?{key: CryptoKey}} e2e - E2E encryption state, or null/undefined for plain
 *   Populated for relay r/o viewers. Triggers client-side clipping + resize
 * @returns {object} Object containing WebSocket instances and cleanup function
 */
export function initWebSockets(
    webClientId,
    sessionName,
    term,
    fitAddon,
    sendAnsiKey,
    e2e,
    roViewer,
    handshake,
    controlChannel,
    bufferedControl
) {
    const handshakeQuery = handshake
        ? `&handshake=${encodeURIComponent(handshake)}`
        : "";
    let ownWebClientId = "";
    let wsTerminal;
    let wsControl;
    const bootConfig = (roViewer && roViewer.config) || null;
    const userConfig = { blink: false, style: false };
    if (bootConfig) {
        if (typeof bootConfig.cursor_blink !== "undefined") {
            userConfig.blink = true;
        }
        if (typeof bootConfig.cursor_style !== "undefined") {
            userConfig.style = true;
        }
    }
    const textDecoder = new TextDecoder();
    const textEncoder = new TextEncoder();

    const relay = e2e && e2e.relay ? e2e.relay : null;
    let terminalOutSeq = 0;
    let terminalSendChain = Promise.resolve();
    const makeWindow = () => {
        let last = -1;
        return (seq) => {
            if (seq <= last) {
                return false;
            }
            last = seq;
            return true;
        };
    };
    const terminalInWindow = makeWindow();
    const controlSend = controlChannel
        ? (obj) => controlChannel.send(obj)
        : (obj) => {
              if (wsControl) {
                  wsControl.send(JSON.stringify(obj));
              }
          };

    const isReadOnly = !!(roViewer && roViewer.isReadOnly);
    const isRelayWatcher = isReadOnly && isRelayMode();
    // Clipping is only used for the legacy shared-stream fan-out, which is
    // signalled by a non-zero session size. Under the E2E per-viewer model
    // each read-only viewer gets its own correctly-sized render stream, so
    // the relay reports size 0 and we render it like any normal client —
    // input is still gated below by `isReadOnly`.
    const useClipper = isReadOnly && !!(roViewer && (roViewer.sessionRows || 0) > 0);
    let clipper = null;
    const pendingFrames = [];
    let clipperReady = false;

    if (useClipper) {
        const baseUrl = `${getBaseUrl()}/`;
        // 0 sentinels mean "session size not yet known at login time" —
        // use a reasonable default and let the first `SessionSizeChanged`
        // control message overwrite.
        const initialRows = roViewer.sessionRows || 24;
        const initialCols = roViewer.sessionCols || 80;
        createClipper(baseUrl, initialRows, initialCols)
            .then((c) => {
                clipper = c;
                clipperReady = true;
                for (const buf of pendingFrames) {
                    clipper.apply(buf);
                }
                pendingFrames.length = 0;
                term.write(clipper.emit(term.rows, term.cols));
            })
            .catch((err) => {
                console.error("clip.wasm load failed:", err);
            });
    }

    const fitDimensions = fitAddon.proposeDimensions() || {
        rows: term.rows,
        cols: term.cols,
    };
    if (fitDimensions.rows !== term.rows || fitDimensions.cols !== term.cols) {
        term.resize(fitDimensions.cols, fitDimensions.rows);
    }

    if (controlChannel) {
        adoptControlChannel(
            controlChannel,
            term,
            fitAddon,
            webClientId,
            userConfig,
            isReadOnly,
            () => clipper,
            controlSend
        );
    }

    const wsBaseUrl = getWsApiBase();
    const url =
        !sessionName || sessionName === ""
            ? `${wsBaseUrl}/ws/terminal`
            : `${wsBaseUrl}/ws/terminal/${sessionName}`;

    let queryString = `?web_client_id=${encodeURIComponent(webClientId)}&rows=${term.rows}&cols=${term.cols}`;
    const bootCell = getCellPixelDimensions(term);
    if (bootCell) {
        const cellWidth = Math.round(bootCell.width);
        const cellHeight = Math.round(bootCell.height);
        queryString += `&cell_width=${cellWidth}&cell_height=${cellHeight}`;
        lastSentCellDimensions = { width: cellWidth, height: cellHeight };
    }
    queryString += handshakeQuery;
    const wsTerminalUrl = `${url}${queryString}`;

    // The control socket is opened up front so the server never has to hold
    // back state it produces before the browser is listening. It carries no
    // STDIN; keystroke transmission stays gated on the first decrypted frame
    // via `ownWebClientId` below. In relay mode the channel already exists
    // (opened for admission before the application bundle loaded) and is
    // adopted above instead.
    if (!controlChannel) {
        const wsControlUrl = `${wsBaseUrl}/ws/control?web_client_id=${encodeURIComponent(
            webClientId
        )}${handshakeQuery}`;
        wsControl = new WebSocket(wsControlUrl);
        wsControl.binaryType = "arraybuffer";
        startWsControl(
            wsControl,
            term,
            fitAddon,
            webClientId,
            userConfig,
            isReadOnly,
            () => clipper,
            controlSend
        );
    }

    window.__zjSendControl = function (payload) {
        const controlClientId = ownWebClientId || webClientId;
        if (!controlClientId || !payload) {
            return;
        }
        if (isRelayWatcher) {
            return;
        }
        if (isReadOnly && !READ_ONLY_ALLOWED_CONTROL.has(payload.type)) {
            return;
        }
        controlSend({ web_client_id: controlClientId, payload });
    };

    wsTerminal = new WebSocket(wsTerminalUrl);
    // With E2E on, the server emits ciphertext as binary frames; default
    // Blob type would make decryption awkward. With no E2E, binary frames
    // are never produced, so setting this is safe either way.
    wsTerminal.binaryType = "arraybuffer";

    wsTerminal.onopen = function () {
        markConnectionEstablished();
    };

    const handleTerminalFrame = async function (event) {
        let data = event.data;
        // Under r/o, keep the raw plaintext bytes separately so they can
        // feed the clipper directly (avoids a UTF-8 round-trip).
        let roPlaintext = null;

        // Phase 3 client-commitment rule: under E2E, the first STDIN
        // byte must never be transmitted before we have successfully
        // decrypted at least one server frame. `ownWebClientId` gates
        // `sendAnsiKey`, so leave it empty until a clean decrypt.
        if (relay) {
            if (!(data instanceof ArrayBuffer)) {
                console.error(
                    "received plaintext frame under E2E; refusing to activate STDIN"
                );
                return;
            }
            try {
                const { seq, plaintext } = await decryptSeq(
                    relay.terminalS2v,
                    FRAME_TYPE_TERMINAL,
                    DIRECTION_SHARER_TO_VIEWER,
                    data
                );
                if (!terminalInWindow(seq)) {
                    return;
                }
                if (useClipper) {
                    roPlaintext = new Uint8Array(plaintext);
                }
                data = textDecoder.decode(plaintext);
            } catch (err) {
                console.error("e2e decrypt failed:", err);
                return;
            }
        } else if (e2e) {
            if (!(data instanceof ArrayBuffer)) {
                // Under E2E, any Text frame from the server is a
                // protocol violation: the server always emits Binary
                // ciphertext. Refuse to activate STDIN.
                console.error(
                    "received plaintext frame under E2E; refusing to activate STDIN"
                );
                return;
            }
            try {
                const plaintext = await decrypt(e2e.key, data);
                if (useClipper) {
                    roPlaintext = new Uint8Array(plaintext);
                }
                data = textDecoder.decode(plaintext);
            } catch (err) {
                console.error("e2e decrypt failed:", err);
                return;
            }
        } else if (useClipper) {
            if (data instanceof ArrayBuffer) {
                roPlaintext = new Uint8Array(data);
            } else if (typeof data === "string") {
                roPlaintext = textEncoder.encode(data);
            }
        }

        // Activate STDIN and the control WS only after the first frame
        // has arrived (and, under E2E, decrypted cleanly). A decrypt
        // failure or protocol violation above returned early without
        // setting `ownWebClientId`, so a second chance is available
        // when the next frame arrives.
        if (ownWebClientId == "") {
            ownWebClientId = webClientId;
        }

        if (useClipper && roPlaintext) {
            // Route the raw server-serialized ANSI stream through the
            // clipper. xterm gets a freshly re-emitted stream sized to
            // the viewer's viewport — no network traffic on local
            // resize (see `setupResizeHandler`) and no passthrough of
            // title/cursor sequences since the clipper normalises them.
            if (!clipperReady) {
                pendingFrames.push(roPlaintext);
                return;
            }
            clipper.apply(roPlaintext);
            term.write(clipper.emit(term.rows, term.cols));
            return;
        }

        if (typeof data === "string") {
            // Handle ANSI title change sequences
            const titleRegex = /\x1b\]0;([^\x07\x1b]*?)(?:\x07|\x1b\\)/g;
            let match;
            while ((match = titleRegex.exec(data)) !== null) {
                document.title = match[1];
            }

            if ((userConfig.blink || userConfig.style) && (
                data.includes("\x1b[0 q") ||
                data.includes("\x1b[1 q") ||
                data.includes("\x1b[2 q") ||
                data.includes("\x1b[3 q") ||
                data.includes("\x1b[4 q") ||
                data.includes("\x1b[5 q") ||
                data.includes("\x1b[6 q")
            )) {
                data = data.replace(/\x1b\[([0-6]) q/g, (match, p1) => {
                    const id = parseInt(p1);

                    // Decode app-requested blink and shape from DECSCUSR id
                    // id 0 = reset-to-default (null = no preference)
                    const appBlink = id === 0 ? null : (id % 2 === 1);
                    const appShapes = [null, "block", "block", "underline", "underline", "bar", "bar"];
                    const appShape  = appShapes[id];

                    // Apply user overrides only for what was explicitly configured;
                    // otherwise pass through the app's value (or fall back to term.options)
                    const effectiveBlink = userConfig.blink ? term.options.cursorBlink
                                                            : (appBlink !== null ? appBlink : term.options.cursorBlink);
                    const effectiveShape = userConfig.style ? term.options.cursorStyle
                                                            : (appShape !== null ? appShape : term.options.cursorStyle);

                    if (effectiveShape === "block")     return effectiveBlink ? "\x1b[1 q" : "\x1b[2 q";
                    if (effectiveShape === "underline") return effectiveBlink ? "\x1b[3 q" : "\x1b[4 q";
                    if (effectiveShape === "bar")       return effectiveBlink ? "\x1b[5 q" : "\x1b[6 q";
                    return match;
                });
            }
        }

        term.write(data);
    };

    let terminalRecvChain = Promise.resolve();
    wsTerminal.onmessage = function (event) {
        terminalRecvChain = terminalRecvChain
            .then(() => handleTerminalFrame(event))
            .catch((err) => {
                console.error("terminal frame handling failed:", err);
            });
    };

    wsTerminal.onclose = function (event) {
        if (event.code === 4001) {
            handleDisconnected();
        } else {
            handleReconnection();
        }
    };

    // Update sendAnsiKey to use the actual WebSocket.
    // With E2E on, encrypt every outbound payload. xterm emits strings
    // via term.onData and Uint8Arrays via term.onBinary (see input.js);
    // we handle both.
    const originalSendAnsiKey = sendAnsiKey;
    sendAnsiKey = async (rawAnsiKey) => {
        if (ownWebClientId === "") {
            return;
        }
        if (isReadOnly) {
            // Relay drops r/o input at its side; belt-and-braces — never
            // transmit anything from this viewer.
            return;
        }
        let ansiKey = rawAnsiKey;
        if (typeof window.__zjMobileMergeKey === "function") {
            ansiKey = window.__zjMobileMergeKey(ansiKey);
        }
        if (relay || e2e) {
            let bytes;
            if (typeof ansiKey === "string") {
                bytes = new TextEncoder().encode(ansiKey);
            } else if (ansiKey instanceof Uint8Array) {
                bytes = ansiKey;
            } else if (ansiKey instanceof ArrayBuffer) {
                bytes = new Uint8Array(ansiKey);
            } else {
                console.error("sendAnsiKey: unsupported payload type", ansiKey);
                return;
            }
            if (relay) {
                terminalSendChain = terminalSendChain.then(async () => {
                    try {
                        const ct = await encryptSeq(
                            relay.terminalV2s,
                            terminalOutSeq,
                            FRAME_TYPE_TERMINAL,
                            DIRECTION_VIEWER_TO_SHARER,
                            bytes
                        );
                        terminalOutSeq += 1;
                        wsTerminal.send(ct);
                    } catch (err) {
                        console.error("e2e encrypt failed:", err);
                    }
                });
                return;
            }
            try {
                const ct = await encrypt(e2e.key, bytes);
                wsTerminal.send(ct);
            } catch (err) {
                console.error("e2e encrypt failed:", err);
            }
            return;
        }
        wsTerminal.send(ansiKey);
    };

    setupResizeHandler(
        term,
        fitAddon,
        controlSend,
        () => ownWebClientId,
        isReadOnly,
        () => clipper
    );

    return {
        wsTerminal,
        getWsControl: () => wsControl,
        getOwnWebClientId: () => ownWebClientId,
        sendAnsiKey,
        cleanup: () => {
            if (wsTerminal) {
                wsTerminal.close();
            }
            if (wsControl) {
                wsControl.close();
            }
        },
    };
}

function sendInitialControl(controlSend, controlClientId, term, fitAddon, isReadOnly) {
    controlSend({
        web_client_id: controlClientId,
        payload: { type: "ClientReady" },
    });
    if (isReadOnly) {
        return;
    }
    const fitDimensions = fitAddon.proposeDimensions();
    if (!fitDimensions) {
        return;
    }
    const { rows, cols } = fitDimensions;
    sendSizeUpdate(controlSend, controlClientId, term, rows, cols);
}

function adoptControlChannel(
    controlChannel,
    term,
    fitAddon,
    controlClientId,
    userConfig,
    isReadOnly,
    getClipper,
    controlSend
) {
    sendInitialControl(controlSend, controlClientId, term, fitAddon, isReadOnly);
    controlChannel.onFrame((msg) =>
        handleControlMessage(msg, {
            term,
            fitAddon,
            controlClientId,
            userConfig,
            isReadOnly,
            getClipper,
            controlSend,
        })
    );
}

function startWsControl(
    wsControl,
    term,
    fitAddon,
    controlClientId,
    userConfig,
    isReadOnly,
    getClipper,
    controlSend
) {
    wsControl.onopen = function (event) {
        sendInitialControl(controlSend, controlClientId, term, fitAddon, isReadOnly);
    };

    wsControl.onmessage = function (event) {
        const msg = JSON.parse(event.data);
        handleControlMessage(msg, {
            term,
            fitAddon,
            controlClientId,
            userConfig,
            isReadOnly,
            getClipper,
            controlSend,
        });
    };

    wsControl.onclose = function (event) {
        if (event.code === 4001) {
            handleDisconnected();
        } else {
            handleReconnection();
        }
    };
}

function handleControlMessage(msg, ctx) {
    const {
        term,
        fitAddon,
        controlClientId,
        userConfig,
        isReadOnly,
        getClipper,
        controlSend,
    } = ctx;
    if (msg.type === "SetConfig") {
            const options = terminalConfigOptions(msg);
            for (const key of Object.keys(options)) {
                term.options[key] = options[key];
            }
            if (typeof msg.cursor_blink !== "undefined") {
                userConfig.blink = true;
            }
            if (typeof msg.cursor_style !== "undefined") {
                userConfig.style = true;
            }
            if (typeof window.__zjSyncInactiveCursorStyle === "function") {
                window.__zjSyncInactiveCursorStyle();
            }
            applyTerminalBackground(msg);

            if (isReadOnly) {
                const clipper = getClipper ? getClipper() : null;
                if (clipper) {
                    term.write(clipper.emit(term.rows, term.cols));
                }
            }
        } else if (msg.type === "QueryTerminalSize") {
            const sizing = getMobileRenderSizing();
            if (sizing.pinned) {
                if (sizing.rows !== term.rows || sizing.cols !== term.cols) {
                    term.resize(sizing.cols, sizing.rows);
                }
                if (!isReadOnly) {
                    sendSizeUpdate(
                        controlSend,
                        controlClientId,
                        term,
                        sizing.rows,
                        sizing.cols
                    );
                }
            } else {
                const fitDimensions = fitAddon.proposeDimensions();
                const { rows, cols } = fitDimensions;
                if (rows !== term.rows || cols !== term.cols) {
                    term.resize(cols, rows);
                }
                if (!isReadOnly) {
                    sendSizeUpdate(controlSend, controlClientId, term, rows, cols);
                }
            }
        } else if (msg.type === "Log") {
            const { lines } = msg;
            for (const line in lines) {
                console.log(line);
            }
        } else if (msg.type === "LogError") {
            const { lines } = msg;
            for (const line in lines) {
                console.error(line);
            }
        } else if (msg.type === "SwitchedSession") {
            const { new_session_name } = msg;
            if (isRelayMode()) {
                document.title = new_session_name;
            } else {
                const baseUrl = getBaseUrl();
                const target = `${baseUrl}/${encodeURIComponent(new_session_name)}`;
                if (!isCurrentLocation(target)) {
                    history.pushState(null, "", target);
                    document.title = new_session_name;
                }
            }
        } else if (msg.type === "SetSoftKeyboard") {
            const { on } = msg;
            setSoftKeyboard(term, !!on);
        } else if (msg.type === "MobileState") {
            const { payload } = msg;
            if (payload) {
                window.__zjLastMobileState = payload;
                if (window.__zjMobileUi) {
                    window.__zjMobileUi.setData(payload);
                }
            }
        } else if (msg.type === "SessionSizeChanged") {
            // Relay-forwarded sharer-side resize. Update the clipper's
            // session grid and re-emit at the viewer's viewport so the
            // terminal paints the new layout in one cycle.
            const clipper = getClipper ? getClipper() : null;
            if (clipper) {
                clipper.resizeSession(Number(msg.rows) || 0, Number(msg.cols) || 0);
                term.write(clipper.emit(term.rows, term.cols));
            }
        }
}

export function setupResizeHandler(
    term,
    fitAddon,
    controlSend,
    getOwnWebClientId,
    isReadOnly,
    getClipper
) {
    let resizeScheduled = false;
    let pendingResizeSignal = false;

    const updateViewportVars = () => {
        const root = document.documentElement;
        const viewport = window.visualViewport;
        const height = viewport ? viewport.height : window.innerHeight;
        const width = viewport ? viewport.width : window.innerWidth;
        root.style.setProperty("--dynamic-vh", `${height}px`);
        root.style.setProperty("--dynamic-vw", `${width}px`);
    };

    const resizeTerminal = () => {
        const ownWebClientId = getOwnWebClientId();
        if (ownWebClientId === "") {
            return;
        }

        const sizing = getMobileRenderSizing();

        if (sizing.pinned) {
            if (sizing.rows !== term.rows || sizing.cols !== term.cols) {
                term.resize(sizing.cols, sizing.rows);
            }
            if (window.__zjMobilePan) {
                window.__zjMobilePan.recompute();
            }
            return;
        }

        const fitDimensions = fitAddon.proposeDimensions();
        if (fitDimensions === undefined) {
            console.warn("failed to get new fit dimensions");
            return;
        }

        const { rows, cols } = fitDimensions;
        if (rows === term.rows && cols === term.cols) {
            return;
        }

        term.resize(cols, rows);

        if (isReadOnly) {
            // Pure client-side re-clip. The sharer's session viewport
            // has not changed; only our cut of it has.
            const clipper = getClipper ? getClipper() : null;
            if (clipper) {
                term.write(clipper.emit(rows, cols));
            }
            return;
        }

        sendSizeUpdate(controlSend, ownWebClientId, term, rows, cols);
    };

    const handleViewportChange = () => {
        updateViewportVars();
        resizeTerminal();
    };

    const scheduleResize = () => {
        pendingResizeSignal = true;
        if (resizeScheduled) {
            return;
        }
        resizeScheduled = true;
        requestAnimationFrame(() => {
            resizeScheduled = false;
            if (!pendingResizeSignal) {
                return;
            }
            pendingResizeSignal = false;
            handleViewportChange();
        });
    };

    updateViewportVars();
    addEventListener("resize", scheduleResize);
    if (window.visualViewport) {
        window.visualViewport.addEventListener("resize", scheduleResize);
    }
    addEventListener("zellij:rendering-resize", scheduleResize);

    setupSoftKeyboardVisibilityTracker(
        controlSend,
        getOwnWebClientId,
        isReadOnly && isRelayMode()
    );
}

function setupSoftKeyboardVisibilityTracker(
    controlSend,
    getOwnWebClientId,
    isRelayWatcher
) {
    if (!window.visualViewport) {
        return;
    }
    const VIEWPORT_DELTA_THRESHOLD_PX = 150;
    let lastViewportHeight = window.visualViewport.height;
    let kbdVisible = false;

    const onResize = () => {
        const newHeight = window.visualViewport.height;
        const delta = newHeight - lastViewportHeight;
        let newKbdVisible = kbdVisible;
        if (delta < -VIEWPORT_DELTA_THRESHOLD_PX) {
            newKbdVisible = true;
        } else if (delta > VIEWPORT_DELTA_THRESHOLD_PX) {
            newKbdVisible = false;
        }
        lastViewportHeight = newHeight;
        if (newKbdVisible === kbdVisible) {
            return;
        }
        kbdVisible = newKbdVisible;

        window.dispatchEvent(
            new CustomEvent("zellij:soft-keyboard-visibility", {
                detail: { visible: kbdVisible },
            })
        );

        if (!kbdVisible) {
            const capture =
                window.__zjSoftKbdCapture &&
                window.__zjSoftKbdCapture.element;
            if (capture && window.__zjSoftKbdCapture.isFocused) {
                capture.blur();
            }
        }

        const ownWebClientId = getOwnWebClientId();
        if (ownWebClientId === "" || isRelayWatcher) {
            return;
        }
        controlSend({
            web_client_id: ownWebClientId,
            payload: {
                type: "SoftKeyboardVisibilityChanged",
                visible: kbdVisible,
            },
        });
    };

    window.visualViewport.addEventListener("resize", onResize);
}
