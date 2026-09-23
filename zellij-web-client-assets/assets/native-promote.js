/**
 * Native-client promotion banner for the relay viewer.
 *
 * The native client (`zellij attach <url>`) is the strongest tier: its
 * crypto runs in the local binary and it never loads app-origin code, so it
 * holds even against a malicious relay. This surfaces the exact command
 * derived from the current join URL (fragment secret included) with a copy
 * button. Shown only on the relay viewer; never on the local web client.
 */

import { getRelayTarget, isRelayMode } from "/assets/utils.js";

function nativeCommand() {
    return `zellij attach "${location.href}"`;
}

function isOneTimeLink() {
    return getRelayTarget().linkId !== null;
}

function mount() {
    if (!isRelayMode()) {
        return;
    }
    if (document.getElementById("zellij-native-promote")) {
        return;
    }

    const bar = document.createElement("div");
    bar.id = "zellij-native-promote";

    const label = document.createElement("span");
    label.className = "znp-label";
    label.textContent = "Open in Zellij (recommended for sensitive sessions):";

    const note = document.createElement("span");
    note.className = "znp-note";
    note.textContent = "The native client holds even against a malicious relay.";

    const content = [label];
    if (isOneTimeLink()) {
        note.textContent += " To open this session in Zellij, ask the host for a new link.";
    } else {
        const code = document.createElement("code");
        code.className = "znp-cmd";
        code.textContent = nativeCommand();

        const copy = document.createElement("button");
        copy.className = "znp-copy";
        copy.type = "button";
        copy.textContent = "Copy";
        copy.addEventListener("click", async () => {
            try {
                await navigator.clipboard.writeText(nativeCommand());
                copy.textContent = "Copied";
            } catch (_) {
                copy.textContent = "Copy failed";
            }
        });
        content.push(code, copy);
    }
    content.push(note);

    const dismiss = document.createElement("button");
    dismiss.className = "znp-dismiss";
    dismiss.type = "button";
    dismiss.setAttribute("aria-label", "Dismiss");
    dismiss.textContent = "✕";
    dismiss.addEventListener("click", () => bar.remove());

    bar.append(...content, dismiss);
    document.body.prepend(bar);
}

if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", mount);
} else {
    mount();
}
