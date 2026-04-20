/**
 * WebSocket management for terminal and control connections
 */

import { handleReconnection, handleDisconnected, markConnectionEstablished } from "./connection.js";
import { getBaseUrl, getWebSocketBaseUrl } from "./utils.js";
import { encrypt, decrypt } from "./crypto.js";

/**
 * Initialize both terminal and control WebSocket connections
 * @param {string} webClientId - Client ID from authentication
 * @param {string} sessionName - Session name from URL
 * @param {Terminal} term - Terminal instance
 * @param {FitAddon} fitAddon - Terminal fit addon
 * @param {function} sendAnsiKey - Function to send ANSI key sequences
 * @param {?{key: CryptoKey}} e2e - E2E encryption state, or null/undefined for plain
 * @returns {object} Object containing WebSocket instances and cleanup function
 */
export function initWebSockets(
    webClientId,
    sessionName,
    term,
    fitAddon,
    sendAnsiKey,
    e2e
) {
    let ownWebClientId = "";
    let wsTerminal;
    let wsControl;
    const userConfig = { blink: false, style: false };
    const textDecoder = new TextDecoder();

    const wsBaseUrl = getWebSocketBaseUrl();
    const url =
        sessionName === ""
            ? `${wsBaseUrl}/ws/terminal`
            : `${wsBaseUrl}/ws/terminal/${sessionName}`;

    const queryString = `?web_client_id=${encodeURIComponent(webClientId)}`;
    const wsTerminalUrl = `${url}${queryString}`;

    wsTerminal = new WebSocket(wsTerminalUrl);
    // With E2E on, the server emits ciphertext as binary frames; default
    // Blob type would make decryption awkward. With no E2E, binary frames
    // are never produced, so setting this is safe either way.
    wsTerminal.binaryType = "arraybuffer";

    wsTerminal.onopen = function () {
        markConnectionEstablished();
    };

    wsTerminal.onmessage = async function (event) {
        let data = event.data;

        // Phase 3 client-commitment rule: under E2E, the first STDIN
        // byte must never be transmitted before we have successfully
        // decrypted at least one server frame. `ownWebClientId` gates
        // `sendAnsiKey`, so leave it empty until a clean decrypt.
        if (e2e) {
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
                data = textDecoder.decode(plaintext);
            } catch (err) {
                console.error("e2e decrypt failed:", err);
                return;
            }
        }

        // Activate STDIN and the control WS only after the first frame
        // has arrived (and, under E2E, decrypted cleanly). A decrypt
        // failure or protocol violation above returned early without
        // setting `ownWebClientId`, so a second chance is available
        // when the next frame arrives.
        if (ownWebClientId == "") {
            ownWebClientId = webClientId;
            const wsControlUrl = `${wsBaseUrl}/ws/control`;
            wsControl = new WebSocket(wsControlUrl);
            startWsControl(wsControl, term, fitAddon, ownWebClientId, userConfig);
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
    sendAnsiKey = async (ansiKey) => {
        if (ownWebClientId === "") {
            return;
        }
        if (e2e) {
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

    // Setup resize handler
    setupResizeHandler(
        term,
        fitAddon,
        () => wsControl,
        () => ownWebClientId
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

/**
 * Start the control WebSocket and set up its handlers
 * @param {WebSocket} wsControl - Control WebSocket instance
 * @param {Terminal} term - Terminal instance
 * @param {FitAddon} fitAddon - Terminal fit addon
 * @param {string} ownWebClientId - Own web client ID
 */
function startWsControl(wsControl, term, fitAddon, ownWebClientId, userConfig) {
    wsControl.onopen = function (event) {
        const fitDimensions = fitAddon.proposeDimensions();
        const { rows, cols } = fitDimensions;
        wsControl.send(
            JSON.stringify({
                web_client_id: ownWebClientId,
                payload: {
                    type: "TerminalResize",
                    rows,
                    cols,
                },
            })
        );
    };

    wsControl.onmessage = function (event) {
        const msg = JSON.parse(event.data);
        if (msg.type === "SetConfig") {
            const {
                font,
                theme,
                cursor_blink,
                mac_option_is_meta,
                cursor_style,
                cursor_inactive_style,
            } = msg;
            term.options.fontFamily = font;
            term.options.theme = theme;
            if (cursor_blink !== "undefined") {
                term.options.cursorBlink = cursor_blink;
                userConfig.blink = true;
            }
            if (mac_option_is_meta !== "undefined") {
                term.options.macOptionIsMeta = mac_option_is_meta;
            }
            if (cursor_style !== "undefined") {
                term.options.cursorStyle = cursor_style;
                userConfig.style = true;
            }
            if (cursor_inactive_style !== "undefined") {
                term.options.cursorInactiveStyle = cursor_inactive_style;
            }
            const body = document.querySelector("body");
            body.style.background = theme.background || "black";

            const terminal = document.getElementById("terminal");
            terminal.style.background = theme.background;

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

            wsControl.send(
                JSON.stringify({
                    web_client_id: ownWebClientId,
                    payload: {
                        type: "TerminalResize",
                        rows,
                        cols,
                    },
                })
            );
        } else if (msg.type === "QueryTerminalSize") {
            const fitDimensions = fitAddon.proposeDimensions();
            const { rows, cols } = fitDimensions;
            if (rows !== term.rows || cols !== term.cols) {
                term.resize(cols, rows);
            }
            wsControl.send(
                JSON.stringify({
                    web_client_id: ownWebClientId,
                    payload: {
                        type: "TerminalResize",
                        rows,
                        cols,
                    },
                })
            );
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
            const baseUrl = getBaseUrl();
            window.location.href = `${baseUrl}/${encodeURIComponent(new_session_name)}`;
        }
    };

    wsControl.onclose = function (event) {
        if (event.code === 4001) {
            handleDisconnected();
        } else {
            handleReconnection();
        }
    };
}

/**
 * Set up window resize event handler
 * @param {Terminal} term - Terminal instance
 * @param {FitAddon} fitAddon - Terminal fit addon
 * @param {function} getWsControl - Function that returns control WebSocket
 * @param {function} getOwnWebClientId - Function that returns own web client ID
 */
export function setupResizeHandler(
    term,
    fitAddon,
    getWsControl,
    getOwnWebClientId
) {
    let resizeScheduled = false;

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

        const wsControl = getWsControl();
        if (wsControl) {
            wsControl.send(
                JSON.stringify({
                    web_client_id: ownWebClientId,
                    payload: {
                        type: "TerminalResize",
                        rows,
                        cols,
                    },
                })
            );
        }
    };

    const handleViewportChange = () => {
        updateViewportVars();
        resizeTerminal();
    };

    const scheduleResize = () => {
        if (resizeScheduled) {
            return;
        }
        resizeScheduled = true;
        requestAnimationFrame(() => {
            resizeScheduled = false;
            handleViewportChange();
        });
    };

    updateViewportVars();
    addEventListener("resize", scheduleResize);
    if (window.visualViewport) {
        window.visualViewport.addEventListener("resize", scheduleResize);
    }
}
