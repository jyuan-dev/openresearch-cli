import assert from "node:assert/strict";
import test from "node:test";
import { act, createElement } from "react";
import { FolderBrowser } from "../src/components/FolderBrowser.tsx";
import { m } from "../src/paraglide/messages.js";
import { find, render, serve, settle } from "./render.mjs";

const click = async (element) => {
  await act(async () => element.click());
  await settle();
};

const listings = {
  "": { path: "/home/me", parent: "/home", home: "/home/me", git: false, folders: [
    { name: "OpenResearch", path: "/home/me/OpenResearch", git: false },
  ] },
  "/home/me/OpenResearch": { path: "/home/me/OpenResearch", parent: "/home/me", home: "/home/me", git: false, folders: [
    { name: "slot-jepa", path: "/home/me/OpenResearch/slot-jepa", git: true },
  ] },
  "/home/me/OpenResearch/slot-jepa": { path: "/home/me/OpenResearch/slot-jepa", parent: "/home/me/OpenResearch", home: "/home/me", git: true, folders: [] },
};

test("the folder browser walks the server's folders and chooses the one it is in", async () => {
  const asked = [];
  serve((path) => {
    if (!path.startsWith("/api/project-path/browse")) return undefined;
    const query = new URLSearchParams(path.split("?")[1] ?? "");
    asked.push(query.get("path") ?? "");
    return listings[query.get("path") ?? ""];
  });
  const chosen = [];
  await render(createElement(FolderBrowser, { onChoose: (path) => chosen.push(path), onCancel: () => {} }));
  await settle();
  assert.ok(document.body.textContent.includes("/home/me"), "starts at home");
  assert.ok(find("button", m.folder_browser_home()).disabled, "already home");

  await click(find(`button[aria-label="${m.folder_browser_open({ name: "OpenResearch" })}"]`));
  await click(find(`button[aria-label="${m.folder_browser_open({ name: "slot-jepa" })}"]`));
  assert.ok(document.body.textContent.includes(m.folder_browser_empty()));
  await click(find("button", m.folder_browser_up()));
  assert.ok(find(`button[aria-label="${m.folder_browser_open({ name: "slot-jepa" })}"]`).textContent.includes(m.folder_browser_git()), "a repository is marked");
  await click(find(`button[aria-label="${m.folder_browser_open({ name: "slot-jepa" })}"]`));
  await click(find("button", m.folder_browser_choose()));
  assert.deepEqual(chosen, ["/home/me/OpenResearch/slot-jepa"]);
  assert.deepEqual(asked, ["", "/home/me/OpenResearch", "/home/me/OpenResearch/slot-jepa"], "visited folders come from the cache");
});

test("a folder that cannot be read says why and offers the way home", async () => {
  serve((path) => {
    if (!path.startsWith("/api/project-path/browse")) return undefined;
    if (path.includes("path=")) return new Response(JSON.stringify({ error: "Cannot read /root: Permission denied" }), { status: 400 });
    return listings[""];
  });
  await render(createElement(FolderBrowser, { start: "/root", onChoose: () => {}, onCancel: () => {} }));
  await settle();
  assert.ok(find('[role="alert"]').textContent.includes("Permission denied"));
  assert.ok(find("button", m.folder_browser_choose()).disabled, "nothing to choose");
  await click(find('[role="alert"] button', m.folder_browser_home()));
  assert.ok(document.body.textContent.includes("OpenResearch"));
});
