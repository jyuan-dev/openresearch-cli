import assert from "node:assert/strict";
import test from "node:test";
import { act } from "react";
import { m } from "../src/paraglide/messages.js";
import { createElement } from "react";
import { BoardView } from "../src/board/BoardView.tsx";
import { find, render, serve, settle, type } from "./render.mjs";

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
async function openBoard({ decisions = [], running = [], machines = [], board = { board: null, epics: [], tickets: [] }, extraRoutes = {} } = {}) {
  const writes = [];
  const chats = [];
  const routes = {
    "GET /api/board/inbox": () => ({ decisions, running, machines }),
    "GET /api/board/projects/p": () => board,
    "GET /api/board/machines": () => ({ machines: machines.map((row) => row.machine) }),
    ...extraRoutes,
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

test("a publish card keeps the project private unless the user allows one publish", async () => {
  const { writes } = await openBoard({
    decisions: [card({ id: "d1", kind: "publish_gate", payload: { what: "orx project share --publish" } }, [])],
  });
  assert.ok(document.body.textContent.includes(m.board_card_publish_title()));
  assert.ok(document.body.textContent.includes("orx project share --publish"));
  assert.ok(find("button", m.board_publish_deny()), "keeping it private is offered");
  await click(find("button", m.board_publish_allow()));
  assert.deepEqual(writes, [{ path: "/api/board/decisions/d1/resolve", body: { option: "allow" } }]);
});

test("a ticket the user asked for in chat is marked as such", async () => {
  await openBoard({
    running: [{ ...ticket("t1", "SJ-004", "lr 1e-3", { status: "running", sessionId: "chat-1", startedAt: Date.now(), proposedBy: "user-chat" }), projectName: project.name }],
  });
  assert.ok(document.body.textContent.includes(m.board_unplanned()));
});

test("an import card sends the grouping as the user settled it", async () => {
  const nodes = {
    a: { title: "E1a robot role head", status: "done", archived: false },
    b: { title: "E1b robot role head", status: "done", archived: false },
    c: { title: "Slot swap diagnostic", status: "cancelled", archived: false },
  };
  const groups = [{ title: "E1: robot role head", experimentIds: ["a", "b"] }, { title: "Unsorted", experimentIds: ["c"] }];
  const { writes } = await openBoard({ decisions: [card({ id: "d1", kind: "import", epicId: null, payload: { groups, nodes } }, [])] });
  assert.ok(document.body.textContent.includes(m.board_card_import_title({ count: "3" })));
  const names = document.querySelectorAll(`input[aria-label="${m.board_import_group_title_label()}"]`);
  await type(names[0], "Robot role");
  // Opening the unsorted group shows its node; moving it into the first group empties the second.
  await click(find("button", m.board_import_group_count({ count: "1" })));
  assert.ok(document.body.textContent.includes("Slot swap diagnostic"));
  const target = document.querySelector(`select[aria-label="${m.board_import_move_label()}"]`);
  const { set } = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value");
  await act(async () => {
    set.call(target, "0");
    target.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await click(find("button", m.board_import_confirm({ count: "1" })));
  assert.deepEqual(writes, [{ path: "/api/board/decisions/d1/resolve", body: { option: "import", groups: [{ title: "Robot role", experimentIds: ["a", "b", "c"] }] } }]);
});

test("giving two groups the same name merges them on import", async () => {
  const nodes = { a: { title: "A", status: "done", archived: false }, b: { title: "B", status: "done", archived: false } };
  const groups = [{ title: "One", experimentIds: ["a"] }, { title: "Two", experimentIds: ["b"] }];
  const { writes } = await openBoard({ decisions: [card({ id: "d1", kind: "import", epicId: null, payload: { groups, nodes } }, [])] });
  await type(document.querySelectorAll(`input[aria-label="${m.board_import_group_title_label()}"]`)[1], "One ");
  await click(find("button", m.board_import_confirm({ count: "1" })));
  assert.deepEqual(writes[0].body.groups, [{ title: "One", experimentIds: ["a", "b"] }]);
});

test("a blocked ticket can be pinned to the machine where its data is", async () => {
  const blocked = { ...ticket("t1", "SJ-047", "role decoder"), status: "blocked", machineId: "ws", pinnedMachineId: null };
  const machine = (id, name) => ({ machine: { id, name, backend: id === "ws" ? "local" : `ssh:${name}`, gpuSlots: 4, own: true, enabled: true }, inUse: 0 });
  const { writes } = await openBoard({
    machines: [machine("ws", "workstation"), machine("n50", "Node50")],
    board: {
      board: { projectId: "p", enabled: true, priority: 100, confidential: true, keyPrefix: "SJ" },
      epics: [],
      tickets: [blocked],
    },
    extraRoutes: { "GET /api/board/tickets/t1": () => ({ ticket: blocked, events: [], epic: null }) },
  });
  await click(find('[role="tab"]', m.board_tab_project()));
  await click(find("button[aria-expanded]", "SJ-047"));
  const select = find("label", m.board_pin_label()).querySelector("select");
  assert.equal(select.value, "", "unpinned: any machine");
  assert.deepEqual([...select.options].map((option) => option.textContent), [m.board_pin_any(), "workstation", "Node50"]);
  await act(async () => {
    select.value = "n50";
    select.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await settle();
  assert.deepEqual(writes, [{ path: "/api/board/tickets/t1/pin", body: { machine: "n50" } }]);
});

test("waiting and running tickets say how many GPUs they take and where they hold them", async () => {
  const machine = (id, name) => ({ machine: { id, name, backend: `ssh:${name}`, gpuSlots: 4, own: true, enabled: true }, inUse: 0 });
  const asked = ticket("t1", "IJ-320", "actor sweep", { status: "waiting", needsGpus: 2, machineId: "n49" });
  const proposed = ticket("t2", "IJ-321", "renderer ablation", { needsGpus: 3 });
  const live = { ...ticket("t3", "SJ-051", "role decoder", { status: "running", needsGpus: 4, machineId: "n50" }), projectName: "Slot-JEPA" };
  const notes = { ...ticket("t4", "SJ-054", "write-up", { kind: "analysis", status: "running", needsGpus: 0 }), projectName: "Slot-JEPA" };
  await openBoard({
    machines: [machine("n49", "Node49"), machine("n50", "Node50")],
    decisions: [
      card({ id: "d1", kind: "question", payload: { question: "Which split?", options: ["val", "test"] } }, [asked]),
      card({ id: "d2", kind: "approve_batch" }, [proposed]),
    ],
    running: [live, notes],
  });
  const text = document.body.textContent;
  assert.ok(text.includes(`${m.board_gpus({ count: "2" })} · Node49`), "a waiting ticket names the machine holding its GPUs");
  assert.ok(text.includes(m.board_gpus({ count: "3" })), "a proposal shows what it would take");
  assert.ok(text.includes(`${m.board_gpus({ count: "4" })} · Node50`), "a running ticket names its machine");
  assert.ok(!text.includes(m.board_gpus({ count: "0" })), "work without GPUs shows no GPU line");
});
