import test from "node:test";
import assert from "node:assert/strict";
import { JSDOM } from "jsdom";
import { deriveRelayHost, getRelayTarget } from "../zellij-web-client-assets/assets/utils.js";

function defineGlobal(name, value) {
    Object.defineProperty(globalThis, name, { value, configurable: true, writable: true });
}

function installPageAt(url, relayOrigin = null) {
    const relayMeta = relayOrigin
        ? `<meta name="zellij-relay-origin" content="${relayOrigin}">`
        : "";
    const page = new JSDOM(
        `<!DOCTYPE html><html><head>${relayMeta}</head><body><input id="zellij-auth-mode" value="relay"></body></html>`,
        { url }
    );
    defineGlobal("window", page.window);
    defineGlobal("document", page.window.document);
    defineGlobal("location", page.window.location);
}

test("the relay host is derived from the page host with the port appended", () => {
    installPageAt("https://app.example.test:8443/r/slug#k=x");
    const target = getRelayTarget();
    assert.equal(target.relayHost, "relay.app.example.test:8443");
    assert.equal(target.httpBase, "https://relay.app.example.test:8443/r/slug");
    assert.equal(target.wsBase, "wss://relay.app.example.test:8443/r/slug");
});

test("a host already starting with relay. is kept unchanged", () => {
    assert.equal(deriveRelayHost("relay.example.com"), "relay.example.com");
});

test("an ip literal host passes through without a relay prefix", () => {
    assert.equal(deriveRelayHost("127.0.0.1"), "127.0.0.1");
    assert.equal(deriveRelayHost("[::1]"), "[::1]");
    installPageAt("http://127.0.0.1:9000/r/slug#k=x");
    assert.equal(getRelayTarget().relayHost, "127.0.0.1:9000");
});

test("a fragment r= override is ignored and the secret still parses", () => {
    installPageAt("http://app.example.test/r/slug#k=x&r=evil.example.net");
    const target = getRelayTarget();
    assert.equal(target.relayHost, "relay.app.example.test");
    assert.equal(target.secret, "x");
});

test("a build-pinned relay origin overrides Pages host derivation", () => {
    installPageAt(
        "https://viewer-project.pages.dev/r/slug#k=local-secret",
        "https://relay.zellij.online"
    );
    const target = getRelayTarget();
    assert.equal(target.relayHost, "relay.zellij.online");
    assert.equal(target.httpBase, "https://relay.zellij.online/r/slug");
    assert.equal(target.wsBase, "wss://relay.zellij.online/r/slug");
    assert.equal(target.secret, "local-secret");
});

test("query and fragment data cannot override the build-pinned target", () => {
    installPageAt(
        "https://app.example.test/r/slug?relay-origin=https://query.evil#k=x&r=fragment.evil&relay-origin=https://fragment.evil",
        "https://relay.neutral.example:9443"
    );
    const target = getRelayTarget();
    assert.equal(target.relayHost, "relay.neutral.example:9443");
    assert.equal(target.httpBase, "https://relay.neutral.example:9443/r/slug");
    assert.equal(target.wsBase, "wss://relay.neutral.example:9443/r/slug");
    assert.equal(target.secret, "x");
});

test("runtime relay metadata rejects insecure remote origins but permits loopback", () => {
    installPageAt("https://app.example.test/r/slug", "http://relay.example.test:9000");
    assert.throws(() => getRelayTarget(), /invalid relay origin/);

    installPageAt("http://localhost:8080/r/slug", "http://127.0.0.1:9000");
    const target = getRelayTarget();
    assert.equal(target.httpBase, "http://127.0.0.1:9000/r/slug");
    assert.equal(target.wsBase, "ws://127.0.0.1:9000/r/slug");
});

test("http pages derive ws bases and https pages derive wss bases", () => {
    installPageAt("http://app.example.test/r/slug#k=x");
    assert.equal(getRelayTarget().wsBase, "ws://relay.app.example.test/r/slug");
    installPageAt("https://app.example.test/r/slug#k=x");
    assert.equal(getRelayTarget().wsBase, "wss://relay.app.example.test/r/slug");
});
