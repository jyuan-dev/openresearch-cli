import assert from "node:assert/strict";
import test from "node:test";
import { act } from "react";
import { m } from "../src/paraglide/messages.js";
import { createElement } from "react";
import { BoardView } from "../src/board/BoardView.tsx";
import { find, render, serve, settle } from "./render.mjs";

const project = { id: "p", name: "Slot JEPA" };
const ticket = (id, key, title, extra = {}) => ({
  id, key, title, kind: "experiment", status: "proposed", outcome: null, conclusion: null,
  hypothesis: `${title} helps`, estMinutes: 90, needsGpus: 1, startedAt: null, sessionId: null,
  projectId: "p", experimentId: null, result: null, ...extra,
});
const card = (decision, tickets) => ({
  decision: { projectId: "p", epicId: "e", ticketIds: tickets.map((row) => row.id), createdAt: Date.now(), payload: {}, ...decision },
  projectName: project.name,
  epic: { id: "e", key: "SJ-E1", title: "Learning rate", question: "Which LR converges fastest?" },
  criteria: null,
  tickets,
});

const click = async (element) => {
  await act(async () => element.click());
  await settle();
};

/** Shows the board with this inbox and project board; returns every write the page sent and every chat it opened. */
async function openBoard({ decisions = [], running = [], machines = [], board = { board: null, epics: [], tickets: [] } } = {}) {
  const writes = [];
  const chats = [];
  const routes = {
    "GET /api/board/inbox": () => ({ decisions, running, machines }),
    "GET /api/board/projects/p": () => board,
    "GET /api/board/machines": () => ({ machines: machines.map((row) => row.machine) }),
  };
  serve((path, init) => {
    const request = `${init.method ?? "GET"} ${path.split("?")[0]}`;
    if (routes[request]) return routes[request](init);
    if (request.startsWith("POST /api/board/")) {
      writes.push({ path: request.slice("POST ".length), body: JSON.parse(init.body) });
      return {};
    }
    return undefined;
  });
  await render(createElement(BoardView, { projectId: "p", onOpenChat: (projectId, sessionId) => chats.push([projectId, sessionId]) }));
  await settle();
  return { writes, chats };
}

test("an empty board says nothing waits and leads to adding a machine", async () => {
  await openBoard();
  assert.ok(find("h1", m.board_title()));
  assert.ok(document.body.textContent.includes(m.board_nothing_waiting()));
  assert.ok(document.body.textContent.includes(m.board_no_machines()));
  await click(find("button", m.board_add_machine_button()));
  assert.ok(find("h3", m.board_add_machine_heading()), "Add machine leads to the machine form");
});

test("unticking a proposal approves only the tickets left selected", async () => {
  const { writes } = await openBoard({
    decisions: [card({ id: "d1", kind: "approve_batch" }, [ticket("t1", "SJ-001", "lr 1e-4"), ticket("t2", "SJ-002", "lr 3e-4")])],
  });
  assert.ok(document.body.textContent.includes(m.board_card_approve_title({ count: "2" })));
  const boxes = document.querySelectorAll('input[type="checkbox"]');
  assert.equal(boxes.length, 2);
  await click(boxes[1]);
  await click(find("button", m.board_approve_selected({ count: "1" })));
  assert.deepEqual(writes, [{ path: "/api/board/decisions/d1/resolve", body: { selected: ["t1"] } }]);
});

test("a question offers the agent's options with its recommendation marked", async () => {
  const asked = ticket("t1", "SJ-001", "lr 1e-4", { status: "waiting" });
  const { writes } = await openBoard({
    decisions: [card({ id: "d1", kind: "question", payload: { question: "Which split?", options: ["val", "test"], recommend: "val" } }, [asked])],
  });
  assert.ok(document.body.textContent.includes("Which split?"));
  const recommended = find("button", "val");
  assert.ok(recommended.textContent.includes(m.board_recommended()));
  assert.ok(!find("button", "test").textContent.includes(m.board_recommended()));
  await click(recommended);
  assert.deepEqual(writes, [{ path: "/api/board/decisions/d1/resolve", body: { option: "val" } }]);
});

test("a project without a board offers to turn it on", async () => {
  const { writes } = await openBoard();
  await click(find('[role="tab"]', m.board_tab_project()));
  assert.ok(find("h3", m.board_off_title()));
  await click(find("button", m.board_turn_on()));
  assert.deepEqual(writes, [{ path: "/api/board/projects/p", body: { enabled: true } }]);
});

test("tickets sit in their status column on an enabled board", async () => {
  await openBoard({
    board: {
      board: { projectId: "p", enabled: true, priority: 100, confidential: true, keyPrefix: "SJ" },
      epics: [],
      tickets: [
        { ...ticket("t1", "SJ-001", "lr 1e-4"), status: "running" },
        { ...ticket("t2", "SJ-002", "lr 3e-4"), status: "review" },
        { ...ticket("t3", "SJ-003", "old idea"), status: "rejected" },
      ],
    },
  });
  await click(find('[role="tab"]', m.board_tab_project()));
  const running = document.querySelector(`section[aria-label="${m.board_status_running()} / ${m.board_status_waiting()}"]`);
  assert.ok(running.textContent.includes("SJ-001"));
  assert.ok(!running.textContent.includes("SJ-002"));
  assert.ok(document.body.textContent.includes(m.board_closed_count({ count: "1" })));
});

test("a running ticket opens its chat", async () => {
  const { chats } = await openBoard({
    running: [{ ...ticket("t1", "SJ-001", "lr 1e-4", { status: "running", sessionId: "chat-1", startedAt: Date.now() }), projectName: project.name }],
  });
  await click(find(`button[aria-label="${m.board_open_chat()}"]`));
  assert.deepEqual(chats, [["p", "chat-1"]]);
});

test("a failed answer stays on its card with the reason", async () => {
  serve(() => undefined);
  const { writes } = await openBoard({
    decisions: [card({ id: "d1", kind: "approve_batch" }, [ticket("t1", "SJ-001", "lr 1e-4")])],
  });
  globalThis.fetch = async () => Response.json({ error: "SJ-001 cannot be approved yet: it still needs approved answer criteria." }, { status: 400 });
  await click(find("button", m.board_approve_selected({ count: "1" })));
  assert.deepEqual(writes, []);
  assert.ok(find('[role="alert"]', "still needs approved answer criteria"));
});
