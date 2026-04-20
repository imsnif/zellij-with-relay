import { initConnectionHandlers } from './connection.js';
import { initAuthentication } from './auth.js';
import { initTerminal } from './terminal.js';
import { setupInputHandlers } from './input.js';
import { initWebSockets } from './websockets.js';

document.addEventListener("DOMContentLoaded", async (event) => {
    initConnectionHandlers();

    const session = await initAuthentication();
    const { webClientId, e2e } = session;

    const { term, fitAddon } = initTerminal();
    const sessionName = location.pathname.split("/").pop();

    let sendAnsiKey = (ansiKey) => {
        // This will be replaced by the WebSocket module
    };

    setupInputHandlers(term, sendAnsiKey);

    document.title = sessionName;
    const websockets = initWebSockets(webClientId, sessionName, term, fitAddon, sendAnsiKey, e2e);

    // Update sendAnsiKey to use the actual WebSocket function returned by initWebSockets
    sendAnsiKey = websockets.sendAnsiKey;

    // Update the input handlers with the correct sendAnsiKey function
    setupInputHandlers(term, sendAnsiKey);
});
