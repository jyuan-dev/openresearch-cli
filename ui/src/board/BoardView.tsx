import { useQuery } from "@tanstack/react-query";
import { ChevronDown, ChevronRight, MessageSquare, Trash2 } from "lucide-react";
import { createContext, useContext, useState, type ReactNode } from "react";
import { m } from "../paraglide/messages.js";
import { autoDir, fmtNumber, ltr } from "../i18n";
import {
  timeAgo,
  type BoardDecisionChoice,
  type BoardImportGroup,
  type BoardInboxCard,
  type BoardOutcome,
  type BoardTicket,
  type BoardTicketStatus,
  type BoardTicketSummary,
  type BoardVerdict,
} from "../api";
import { useWorkspace } from "../workspace";
import { Badge, Button, IconButton, Input, Spinner, Switch, Textarea, Tooltip } from "../components/ui";
import { SEGMENTED_CLASS_NAME } from "../components/ui/Segmented";

const CARD_CLASS_NAME =
  "bg-background border border-border rounded-lg py-4 px-4.5 mb-3 [&_h3]:mt-0 [&_h3]:mx-0 [&_h3]:mb-1 [&_h3]:text-base [&_h3]:font-semibold [&_h3]:text-text";
const SUB_CLASS_NAME = "mt-0 mx-0 mb-3 text-sm leading-relaxed text-subtext";
const ROW_CLASS_NAME = "flex items-start gap-3 py-2.5 border-t border-t-border first:border-t-0";

type Tab = "inbox" | "project" | "machines";

/** Opens a ticket's chat; the app supplies its navigation. */
const OpenChatContext = createContext<(projectId: string, sessionId: string) => void>(() => {});

/** The research board: what waits for the user across projects, this project's tickets, and the machines they run on. */
export function BoardView({ projectId, onOpenChat }: { projectId: string; onOpenChat: (projectId: string, sessionId: string) => void }) {
  const [tab, setTab] = useState<Tab>("inbox");
  // Starts on the open project; any other project's board is one pick away.
  const [boardProject, setBoardProject] = useState(projectId);
  const tabs: [Tab, string][] = [
    ["inbox", m.board_tab_inbox()],
    ["project", m.board_tab_project()],
    ["machines", m.board_tab_machines()],
  ];
  return (
    <div className="settings-view max-w-readable my-0 mx-auto pt-6 px-8 max-sm:px-4 pb-15 [&_h1]:mt-0 [&_h1]:mx-0 [&_h1]:mb-1.5 [&_h1]:text-3xl">
      <h1>{m.board_title()}</h1>
      <p className="mt-0 mx-0 mb-4 text-base leading-relaxed text-text">{m.board_description()}</p>
      <div className={`${SEGMENTED_CLASS_NAME} mb-4`} role="tablist">
        {tabs.map(([id, label]) => (
          <button key={id} role="tab" aria-selected={tab === id} className={tab === id ? "active" : ""} onClick={() => setTab(id)}>
            {label}
          </button>
        ))}
      </div>
      <OpenChatContext value={onOpenChat}>
        {tab === "inbox" ? <Inbox onMachines={() => setTab("machines")} /> : tab === "project" ? (
          <>
            <ProjectPicker value={boardProject} onChange={setBoardProject} />
            <ProjectBoard key={boardProject} projectId={boardProject} />
          </>
        ) : <Machines />}
      </OpenChatContext>
    </div>
  );
}

function useResolve() {
  const { api } = useWorkspace();
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<{ id: string; message: string } | null>(null);
  const resolve = async (decisionId: string, choice: BoardDecisionChoice) => {
    setPending(decisionId);
    setError(null);
    try {
      await api.board.resolve(decisionId, choice);
    } catch (e) {
      setError({ id: decisionId, message: e instanceof Error ? e.message : String(e) });
    } finally {
      setPending(null);
    }
  };
  return { resolve, pending, error };
}

function Inbox({ onMachines }: { onMachines: () => void }) {
  const { queries } = useWorkspace();
  const inbox = useQuery(queries.getBoardInboxQuery());
  const actions = useResolve();
  if (inbox.error) return <LoadError message={inbox.error.message} onRetry={() => void inbox.refetch()} />;
  if (!inbox.data) return <Loading />;
  const { decisions, running, machines } = inbox.data;
  return (
    <>
      <section className={CARD_CLASS_NAME} aria-label={m.board_gpus_heading()}>
        <h3>{m.board_gpus_heading()}</h3>
        {machines.length === 0 ? (
          <div className="flex flex-wrap items-center gap-3 text-sm text-subtext">
            {m.board_no_machines()}
            <Button size="small" onClick={onMachines}>{m.board_add_machine_button()}</Button>
          </div>
        ) : (
          <div className="flex flex-wrap gap-2">
            {machines.map(({ machine, inUse }) => (
              <Badge key={machine.id} variant={inUse >= machine.gpuSlots ? "warning" : "default"}>
                {m.board_machine_usage({ name: autoDir(machine.name), used: fmtNumber(inUse), total: fmtNumber(machine.gpuSlots) })}
              </Badge>
            ))}
          </div>
        )}
      </section>

      <h2 className="mt-5 mb-2 text-lg font-semibold text-text">{m.board_waiting_heading()}</h2>
      {decisions.length === 0 ? (
        <p className={SUB_CLASS_NAME}>{m.board_nothing_waiting()}</p>
      ) : (
        decisions.map((card) => <DecisionCard key={card.decision.id} card={card} actions={actions} />)
      )}

      <h2 className="mt-5 mb-2 text-lg font-semibold text-text">{m.board_running_heading()}</h2>
      {running.length === 0 ? (
        <p className={SUB_CLASS_NAME}>{m.board_nothing_running()}</p>
      ) : (
        <section className={CARD_CLASS_NAME}>
          {running.map((ticket) => (
            <div key={ticket.id} className={ROW_CLASS_NAME}>
              <TicketLine ticket={ticket} project={ticket.projectName} gpus />
              <span className="ms-auto shrink-0 text-xs text-subtext">
                {ticket.startedAt ? m.board_started_ago({ ago: timeAgo(ticket.startedAt) }) : null}
              </span>
              <OpenChatButton ticket={ticket} />
            </div>
          ))}
        </section>
      )}
    </>
  );
}

/** How many GPUs a ticket takes and, while it holds them, on which machine. */
function GpuLine({ ticket }: { ticket: BoardTicketSummary }) {
  const { queries } = useWorkspace();
  const machines = useQuery(queries.getBoardMachinesQuery());
  if (ticket.needsGpus <= 0) return null;
  const holds = (ticket.status === "running" || ticket.status === "waiting") && ticket.machineId;
  const machine = holds ? machines.data?.find((candidate) => candidate.id === ticket.machineId) : undefined;
  return (
    <div className="mt-0.5 text-xs text-subtext">
      {m.board_gpus({ count: fmtNumber(ticket.needsGpus) })}
      {machine && <> · <span dir="auto">{machine.name}</span></>}
    </div>
  );
}

function TicketLine({ ticket, project, gpus = false }: { ticket: BoardTicketSummary; project?: string | null; gpus?: boolean }) {
  return (
    <div className="min-w-0">
      <div className="flex flex-wrap items-center gap-2 text-sm text-text">
        <span className="font-mono text-xs text-subtext">{ltr(ticket.key)}</span>
        <span className="font-medium" dir="auto">{ticket.title}</span>
        <StatusBadge status={ticket.status} />
        {ticket.outcome && <OutcomeBadge outcome={ticket.outcome} />}
        {ticket.proposedBy === "user-chat" && <Badge size="small">{m.board_unplanned()}</Badge>}
      </div>
      {project && <div className="mt-0.5 text-xs text-subtext" dir="auto">{project}</div>}
      {gpus && <GpuLine ticket={ticket} />}
      {ticket.hypothesis && ticket.status === "proposed" && (
        <p className="mt-1 mb-0 text-sm text-text" dir="auto">{ticket.hypothesis}</p>
      )}
      {ticket.conclusion && <p className="mt-1 mb-0 text-sm text-text" dir="auto">{ticket.conclusion}</p>}
    </div>
  );
}

function OpenChatButton({ ticket }: { ticket: Pick<BoardTicketSummary, "sessionId" | "projectId"> }) {
  const openChat = useContext(OpenChatContext);
  if (!ticket.sessionId) return null;
  const sessionId = ticket.sessionId;
  return (
    <Tooltip interactive content={m.board_open_chat()}>
      <IconButton size="small" aria-label={m.board_open_chat()} onClick={() => openChat(ticket.projectId, sessionId)}>
        <MessageSquare size={14} />
      </IconButton>
    </Tooltip>
  );
}

function statusLabel(status: BoardTicketStatus) {
  return {
    proposed: m.board_status_proposed,
    ready: m.board_status_ready,
    running: m.board_status_running,
    waiting: m.board_status_waiting,
    review: m.board_status_review,
    done: m.board_status_done,
    blocked: m.board_status_blocked,
    rejected: m.board_status_rejected,
    cancelled: m.board_status_cancelled,
  }[status]();
}

function StatusBadge({ status }: { status: BoardTicketStatus }) {
  const variant = status === "done" ? "success" : status === "blocked" ? "error" : status === "waiting" || status === "review" ? "warning" : "default";
  return <Badge size="small" variant={variant}>{statusLabel(status)}</Badge>;
}

function OutcomeBadge({ outcome }: { outcome: BoardOutcome }) {
  const label = { supports: m.board_outcome_supports, refutes: m.board_outcome_refutes, inconclusive: m.board_outcome_inconclusive }[outcome]();
  return <Badge size="small" variant={outcome === "supports" ? "success" : outcome === "refutes" ? "error" : "default"}>{label}</Badge>;
}

function VerdictLine({ verdict }: { verdict: BoardVerdict | null | undefined }) {
  if (!verdict) return null;
  return (
    <div className="mt-1 text-xs text-subtext">
      {verdict.mean !== null && verdict.delta !== null && (
        <span>{m.board_verdict_numbers({ mean: fmtNumber(verdict.mean), delta: fmtNumber(verdict.delta) })} </span>
      )}
      {verdict.failures.length > 0 && (
        <ul className="mt-1 mb-0 ps-4 text-accent-amber">
          {verdict.failures.map((failure) => <li key={failure} dir="auto">{failure}</li>)}
        </ul>
      )}
    </div>
  );
}

function CardHeading({ card, title }: { card: BoardInboxCard; title: string }) {
  return (
    <>
      <h3 dir="auto">{title}</h3>
      <p className={SUB_CLASS_NAME} dir="auto">
        {[card.projectName, card.epic ? `${card.epic.key} · ${card.epic.title}` : null].filter(Boolean).join(" · ")}
        {" · "}
        {timeAgo(card.decision.createdAt)}
      </p>
    </>
  );
}

type Actions = ReturnType<typeof useResolve>;

function CardError({ card, actions }: { card: BoardInboxCard; actions: Actions }) {
  if (actions.error?.id !== card.decision.id) return null;
  return <div role="alert" className="mt-2.5 text-sm text-accent-red whitespace-pre-wrap">{actions.error.message}</div>;
}

function Checklist({ card, selected, onToggle, render }: { card: BoardInboxCard; selected: Set<string>; onToggle: (id: string) => void; render?: (ticket: BoardTicketSummary) => ReactNode }) {
  return (
    <div className="mb-3">
      {card.tickets.map((ticket) => (
        <label key={ticket.id} className={`${ROW_CLASS_NAME} cursor-pointer`}>
          <input type="checkbox" className="mt-1" checked={selected.has(ticket.id)} onChange={() => onToggle(ticket.id)} />
          <div className="min-w-0 flex-1">
            <TicketLine ticket={ticket} gpus />
            {render?.(ticket)}
          </div>
        </label>
      ))}
    </div>
  );
}

function DecisionCard({ card, actions }: { card: BoardInboxCard; actions: Actions }) {
  const { api, dialogs } = useWorkspace();
  const { decision } = card;
  const busy = actions.pending === decision.id;
  const [selected, setSelected] = useState(() => new Set(decision.ticketIds));
  const toggle = (id: string) => setSelected((current) => {
    const next = new Set(current);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    return next;
  });
  const resolve = (choice: BoardDecisionChoice) => void actions.resolve(decision.id, choice);
  const ticket = card.tickets[0];
  let body: ReactNode;
  switch (decision.kind) {
    case "approve_batch":
      body = (
        <>
          <CardHeading card={card} title={m.board_card_approve_title({ count: fmtNumber(card.tickets.length) })} />
          <Checklist card={card} selected={selected} onToggle={toggle} render={(row) => (
            <div className="mt-1 text-xs text-subtext">
              {row.estMinutes !== null && m.board_estimate({ minutes: fmtNumber(row.estMinutes) })}
            </div>
          )} />
          <div className="flex flex-wrap gap-2">
            <Button variant="primary" disabled={busy || selected.size === 0} onClick={() => resolve({ selected: [...selected] })}>
              {m.board_approve_selected({ count: fmtNumber(selected.size) })}
            </Button>
            <Button disabled={busy} onClick={() => resolve({ selected: [] })}>{m.board_decline_all()}</Button>
          </div>
        </>
      );
      break;
    case "approve_criteria":
      body = (
        <>
          <CardHeading card={card} title={m.board_card_criteria_title({ version: fmtNumber(decision.payload.version ?? 0) })} />
          {card.epic && <p className="mt-0 mb-2 text-sm font-medium text-text" dir="auto">{card.epic.question}</p>}
          <p className="mt-0 mb-3 text-sm text-text whitespace-pre-wrap" dir="auto">{card.criteria?.bodyMd}</p>
          <Button variant="primary" disabled={busy} onClick={() => resolve({})}>{m.board_approve_criteria()}</Button>
        </>
      );
      break;
    case "accept_batch": {
      const verdicts = decision.payload.verdicts ?? {};
      body = (
        <>
          <CardHeading card={card} title={m.board_card_accept_batch_title({ count: fmtNumber(card.tickets.length) })} />
          <p className={SUB_CLASS_NAME}>{m.board_card_accept_batch_description()}</p>
          <Checklist card={card} selected={selected} onToggle={toggle} render={(row) => <VerdictLine verdict={verdicts[row.id]} />} />
          <Button variant="primary" disabled={busy || selected.size === 0} onClick={() => resolve({ selected: [...selected] })}>
            {m.board_accept_selected({ count: fmtNumber(selected.size) })}
          </Button>
        </>
      );
      break;
    }
    case "accept_one":
      body = ticket && (
        <>
          <CardHeading card={card} title={m.board_card_accept_one_title()} />
          <TicketLine ticket={ticket} gpus />
          <VerdictLine verdict={decision.payload.verdicts?.[ticket.id]} />
          <div className="mt-3 flex flex-wrap gap-2">
            <Button variant="primary" disabled={busy} onClick={() => resolve({ option: "accept" })}>{m.board_accept()}</Button>
            <Button disabled={busy} onClick={() => dialogs.showTextDialog({
              title: m.board_challenge_title(),
              description: m.board_challenge_description(),
              confirmLabel: m.board_challenge_confirm(),
              onConfirm: async (text) => { await api.board.resolve(decision.id, { option: "challenge", text }); },
              onCancel: () => {},
            })}>{m.board_challenge()}</Button>
            <OpenChatButton ticket={ticket} />
          </div>
        </>
      );
      break;
    case "question":
      body = ticket && (
        <>
          <CardHeading card={card} title={m.board_card_question_title({ key: ltr(ticket.key) })} />
          <GpuLine ticket={ticket} />
          <p className="mt-0 mb-3 text-sm text-text whitespace-pre-wrap" dir="auto">{decision.payload.question}</p>
          <div className="flex flex-col items-start gap-2">
            {(decision.payload.options ?? []).map((option) => (
              <Button key={option} variant={option === decision.payload.recommend ? "primary" : "default"} disabled={busy} className="h-auto min-h-8 whitespace-normal py-1.5 text-start" onClick={() => resolve({ option })}>
                <span dir="auto">{option}</span>
                {option === decision.payload.recommend && <Badge size="small">{m.board_recommended()}</Badge>}
              </Button>
            ))}
            <Button variant="ghost" disabled={busy} onClick={() => dialogs.showTextDialog({
              title: m.board_answer_title(),
              description: decision.payload.question ?? "",
              confirmLabel: m.board_answer_confirm(),
              onConfirm: async (text) => { await api.board.resolve(decision.id, { text }); },
              onCancel: () => {},
            })}>{m.board_answer_in_words()}</Button>
          </div>
        </>
      );
      break;
    case "blocked":
      body = ticket && (
        <>
          <CardHeading card={card} title={m.board_card_blocked_title({ key: ltr(ticket.key) })} />
          <TicketLine ticket={ticket} gpus />
          {decision.payload.reason && <p className="mt-2 mb-3 text-sm text-text" dir="auto">{decision.payload.reason}</p>}
          <div className="flex flex-wrap gap-2">
            <Button variant="primary" disabled={busy} onClick={() => resolve({ option: "unblock" })}>{m.board_unblock()}</Button>
            <Button disabled={busy} onClick={() => resolve({ option: "reject" })}>{m.board_decline()}</Button>
            <OpenChatButton ticket={ticket} />
          </div>
        </>
      );
      break;
    case "budget":
      body = (
        <>
          <CardHeading card={card} title={m.board_card_budget_title({ percent: fmtNumber(decision.payload.mark ?? 0) })} />
          <p className={SUB_CLASS_NAME}>
            {m.board_budget_spent({ spent: fmtNumber(Math.round((decision.payload.spentGpuHours ?? 0) * 10) / 10), budget: fmtNumber(decision.payload.budgetGpuHours ?? 0) })}
          </p>
          <Button disabled={busy} onClick={() => resolve({})}>{m.board_budget_seen()}</Button>
        </>
      );
      break;
    case "import":
      body = <ImportBody card={card} busy={busy} onResolve={resolve} />;
      break;
    case "publish_gate":
      body = (
        <>
          <CardHeading card={card} title={m.board_card_publish_title()} />
          <p className={SUB_CLASS_NAME}>{m.board_card_publish_description({ what: decision.payload.what ?? "" })}</p>
          <div className="flex flex-wrap gap-2">
            <Button variant="primary" disabled={busy} onClick={() => resolve({ option: "deny" })}>{m.board_publish_deny()}</Button>
            <Button disabled={busy} onClick={() => resolve({ option: "allow" })}>{m.board_publish_allow()}</Button>
          </div>
        </>
      );
      break;
    default:
      body = <CardHeading card={card} title={decision.kind} />;
  }
  return (
    <section className={CARD_CLASS_NAME} aria-busy={busy}>
      {body}
      <CardError card={card} actions={actions} />
    </section>
  );
}

/** Merges groups given the same name and drops emptied ones, as the import will see them. */
function settleGroups(groups: BoardImportGroup[]): BoardImportGroup[] {
  const merged: BoardImportGroup[] = [];
  for (const group of groups) {
    const title = group.title.trim();
    if (group.experimentIds.length === 0) continue;
    const same = merged.find((seen) => seen.title === title);
    if (same) same.experimentIds.push(...group.experimentIds);
    else merged.push({ title, experimentIds: [...group.experimentIds] });
  }
  return merged;
}

function ImportBody({ card, busy, onResolve }: { card: BoardInboxCard; busy: boolean; onResolve: (choice: BoardDecisionChoice) => void }) {
  const { decision } = card;
  const nodes = decision.payload.nodes ?? {};
  const [groups, setGroups] = useState<BoardImportGroup[]>(() => (decision.payload.groups ?? []).map((group) => ({ ...group, experimentIds: [...group.experimentIds] })));
  const [expanded, setExpanded] = useState<number | null>(null);
  const settled = settleGroups(groups);
  const count = groups.reduce((sum, group) => sum + group.experimentIds.length, 0);
  const rename = (index: number, title: string) => setGroups((current) => current.map((group, i) => (i === index ? { ...group, title } : group)));
  const move = (from: number, id: string, to: number) => setGroups((current) => current.map((group, i) => {
    if (i === from) return { ...group, experimentIds: group.experimentIds.filter((other) => other !== id) };
    if (i === to) return { ...group, experimentIds: [...group.experimentIds, id] };
    return group;
  }));
  return (
    <>
      <CardHeading card={card} title={m.board_card_import_title({ count: fmtNumber(count) })} />
      <p className={SUB_CLASS_NAME}>{m.board_card_import_description()}</p>
      <div className="mb-3">
        {groups.map((group, index) => (
          <div key={index} className={ROW_CLASS_NAME}>
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <Input className="min-w-0 flex-1" value={group.title} aria-label={m.board_import_group_title_label()} onChange={(event) => rename(index, event.target.value)} dir="auto" />
                <Button variant="ghost" aria-expanded={expanded === index} onClick={() => setExpanded(expanded === index ? null : index)}>
                  {m.board_import_group_count({ count: fmtNumber(group.experimentIds.length) })}
                </Button>
              </div>
              {expanded === index && (
                <ul className="mt-2 mb-0 list-none p-0">
                  {group.experimentIds.map((id) => (
                    <li key={id} className="flex flex-wrap items-center gap-2 py-1 text-sm text-text">
                      <span className="min-w-0 basis-full sm:basis-0 sm:flex-1" dir="auto">{nodes[id]?.title ?? id}</span>
                      {nodes[id] && <StatusBadge status={nodes[id].status} />}
                      <select
                        className="h-8 max-w-48 rounded-md border border-border bg-background px-1.5 text-sm text-text"
                        aria-label={m.board_import_move_label()}
                        value={index}
                        onChange={(event) => move(index, id, Number(event.target.value))}
                      >
                        {groups.map((target, targetIndex) => (
                          <option key={targetIndex} value={targetIndex}>{target.title || "…"}</option>
                        ))}
                      </select>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </div>
        ))}
      </div>
      <div className="flex flex-wrap gap-2">
        <Button variant="primary" disabled={busy || settled.some((group) => group.title === "")} onClick={() => onResolve({ option: "import", groups: settled })}>
          {m.board_import_confirm({ count: fmtNumber(settled.length) })}
        </Button>
        <Button disabled={busy} onClick={() => onResolve({ option: "discard" })}>{m.board_import_discard()}</Button>
      </div>
    </>
  );
}

const COLUMNS: BoardTicketStatus[][] = [["proposed"], ["ready"], ["running", "waiting"], ["review", "blocked"], ["done"]];

function ProjectPicker({ value, onChange }: { value: string; onChange: (projectId: string) => void }) {
  const { listProjectsQuery } = useWorkspace().queries;
  const projects = useQuery(listProjectsQuery());
  const rows = [...(projects.data ?? [])].sort((a, b) => a.name.localeCompare(b.name));
  if (rows.length < 2) return null;
  return (
    <label className="mb-4 flex items-center gap-2 text-sm text-text">
      <span className="font-medium">{m.board_project_pick()}</span>
      <select className="min-w-0 max-w-full rounded-md border border-border bg-background px-2 py-1 text-sm" value={value} onChange={(event) => onChange(event.target.value)}>
        {!rows.some((project) => project.id === value) && <option value={value}>{value}</option>}
        {rows.map((project) => <option key={project.id} value={project.id}>{project.name}</option>)}
      </select>
    </label>
  );
}

function ProjectBoard({ projectId }: { projectId: string }) {
  const { api, queries } = useWorkspace();
  const board = useQuery(queries.getProjectBoardQuery(projectId));
  const [open, setOpen] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // Done tickets pile up; their column stays folded until asked for.
  const [doneOpen, setDoneOpen] = useState(false);
  // So do answered and dropped questions; the open ones stay in view.
  const [settledOpen, setSettledOpen] = useState(false);
  if (board.error) return <LoadError message={board.error.message} onRetry={() => void board.refetch()} />;
  if (!board.data) return <Loading />;
  const settings = board.data.board;
  const update = async (change: { enabled?: boolean; confidential?: boolean }) => {
    setError(null);
    try {
      await api.board.updateProject(projectId, change);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  if (!settings?.enabled) {
    return (
      <section className={CARD_CLASS_NAME}>
        <h3>{m.board_off_title()}</h3>
        <p className={SUB_CLASS_NAME}>{m.board_off_description()}</p>
        <Button variant="primary" onClick={() => void update({ enabled: true })}>{m.board_turn_on()}</Button>
        {error && <div role="alert" className="mt-2.5 text-sm text-accent-red">{error}</div>}
      </section>
    );
  }
  const { epics, unticketed } = board.data;
  // Tickets of answered or dropped questions leave the board; their epics still list them.
  const settledEpics = new Set(epics.filter(({ epic }) => epic.status === "answered" || epic.status === "dropped").map(({ epic }) => epic.id));
  const tickets = board.data.tickets.filter((ticket) => !ticket.epicId || !settledEpics.has(ticket.epicId));
  const archived = board.data.tickets.length - tickets.length;
  const epicRow = ({ epic, criteria }: (typeof epics)[number]) => (
    <div key={epic.id} className={ROW_CLASS_NAME}>
      <span className="font-mono text-xs text-subtext pt-0.5">{ltr(epic.key)}</span>
      <div className="min-w-0">
        <div className="text-sm font-medium text-text" dir="auto">{epic.title}</div>
        {epic.question !== epic.title && <div className="text-sm text-subtext" dir="auto">{epic.question}</div>}
      </div>
      <span className="ms-auto shrink-0 text-xs text-subtext">
        {epic.status === "answered" ? m.board_epic_answered() : epic.criteriaVersion > 0 ? m.board_criteria_version({ version: fmtNumber(epic.criteriaVersion) }) : criteria ? m.board_criteria_waiting() : m.board_criteria_none()}
      </span>
    </div>
  );
  const closed = tickets.filter((ticket) => ticket.status === "rejected" || ticket.status === "cancelled").length;
  const draftImport = async () => {
    setError(null);
    setNotice(null);
    try {
      await api.board.proposeImport(projectId);
      setNotice(m.board_import_drafted());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  return (
    <>
      <section className={CARD_CLASS_NAME}>
        <div className="flex flex-wrap items-center gap-x-6 gap-y-2 text-sm text-text">
          <label className="flex items-center gap-2">
            <Switch checked={settings.enabled} onClick={() => void update({ enabled: !settings.enabled })} aria-label={m.board_setting_enabled()} />
            {m.board_setting_enabled()}
          </label>
          <label className="flex items-center gap-2">
            <Switch checked={settings.confidential} onClick={() => void update({ confidential: !settings.confidential })} aria-label={m.board_setting_confidential()} />
            {m.board_setting_confidential()}
          </label>
        </div>
        <p className="mt-2 mb-0 text-xs text-subtext">{m.board_setting_confidential_hint()}</p>
        {error && <div role="alert" className="mt-2.5 text-sm text-accent-red">{error}</div>}
      </section>

      {unticketed > 0 && (
        <section className={`${CARD_CLASS_NAME} mt-3`}>
          <h3>{m.board_import_prompt({ count: fmtNumber(unticketed) })}</h3>
          <p className={SUB_CLASS_NAME}>{m.board_import_prompt_description()}</p>
          <Button onClick={() => void draftImport()}>{m.board_import_draft()}</Button>
          {notice && <p role="status" className="mt-2.5 mb-0 text-sm text-subtext">{notice}</p>}
        </section>
      )}

      <h2 className="mt-5 mb-2 text-lg font-semibold text-text">{m.board_epics_heading()}</h2>
      {epics.length === 0 ? (
        <p className={SUB_CLASS_NAME}>{m.board_no_epics()}</p>
      ) : (
        <section className={CARD_CLASS_NAME}>
          {epics.filter(({ epic }) => !settledEpics.has(epic.id)).map(epicRow)}
          {settledEpics.size > 0 && (
            <button type="button" className="flex w-full items-center gap-1 border-0 bg-transparent px-0 py-2 text-start text-sm text-subtext hover:text-text" aria-expanded={settledOpen} onClick={() => setSettledOpen(!settledOpen)}>
              {settledOpen ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
              {m.board_epics_settled({ count: fmtNumber(settledEpics.size) })}
            </button>
          )}
          {settledOpen && epics.filter(({ epic }) => settledEpics.has(epic.id)).map(epicRow)}
        </section>
      )}

      <h2 className="mt-5 mb-2 text-lg font-semibold text-text">{m.board_tickets_heading()}</h2>
      {tickets.length === 0 ? (
        <p className={SUB_CLASS_NAME}>{m.board_no_tickets()}</p>
      ) : (
        <div className="grid grid-cols-[repeat(auto-fill,minmax(11rem,1fr))] gap-3">
          {COLUMNS.map((statuses) => {
            const column = tickets.filter((ticket) => statuses.includes(ticket.status));
            const foldable = statuses.includes("done");
            const heading = (
              <>
                {statuses.map(statusLabel).join(" / ")} <span className="font-normal text-subtext">{fmtNumber(column.length)}</span>
              </>
            );
            return (
              <section key={statuses.join()} className="min-w-0 rounded-lg bg-surface p-2" aria-label={statuses.map(statusLabel).join(" / ")}>
                <h3 className="mt-0 mb-2 px-1 text-sm font-semibold text-text">
                  {foldable ? (
                    <button type="button" className="flex w-full items-center gap-1 border-0 bg-transparent p-0 text-start text-sm font-semibold text-text" aria-expanded={doneOpen} onClick={() => setDoneOpen(!doneOpen)}>
                      {doneOpen ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                      {heading}
                    </button>
                  ) : heading}
                </h3>
                {(!foldable || doneOpen) && column.map((ticket) => (
                  <button key={ticket.id} className="mb-2 block w-full rounded-md border border-border bg-background p-2 text-start hover:bg-hover-subtle focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-focus focus-visible:outline-offset-2" aria-expanded={open === ticket.id} onClick={() => setOpen(open === ticket.id ? null : ticket.id)}>
                    <div className="flex items-center gap-1.5">
                      <span className="font-mono text-xs text-subtext">{ltr(ticket.key)}</span>
                      {statuses.length > 1 && <StatusBadge status={ticket.status} />}
                      {ticket.tainted && <Badge size="small" variant="error">{m.board_tainted()}</Badge>}
                    </div>
                    <div className="mt-1 text-sm text-text" dir="auto">{ticket.title}</div>
                  </button>
                ))}
              </section>
            );
          })}
        </div>
      )}
      {closed > 0 && <p className="mt-1 text-xs text-subtext">{m.board_closed_count({ count: fmtNumber(closed) })}</p>}
      {archived > 0 && <p className="mt-1 text-xs text-subtext">{m.board_answered_count({ count: fmtNumber(archived) })}</p>}
      {open && <TicketDetail ticketId={open} onClose={() => setOpen(null)} />}
    </>
  );
}

function TicketDetail({ ticketId, onClose }: { ticketId: string; onClose: () => void }) {
  const { api, queries, dialogs } = useWorkspace();
  const detail = useQuery(queries.getBoardTicketQuery(ticketId));
  const [comment, setComment] = useState("");
  const [error, setError] = useState<string | null>(null);
  if (detail.error) return <LoadError message={detail.error.message} onRetry={() => void detail.refetch()} />;
  if (!detail.data) return <Loading />;
  const { ticket, events } = detail.data;
  const act = async (run: () => Promise<unknown>) => {
    setError(null);
    try {
      await run();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  const closedStatuses: BoardTicket["status"][] = ["done", "rejected", "cancelled"];
  return (
    <section className={`${CARD_CLASS_NAME} mt-3`} aria-label={ticket.key}>
      <div className="flex items-start gap-2">
        <TicketLine ticket={ticket} />
        <div className="ms-auto flex shrink-0 gap-1">
          <OpenChatButton ticket={ticket} />
          <Button size="small" variant="ghost" onClick={onClose}>{m.board_close()}</Button>
        </div>
      </div>
      {ticket.changeSpec && <p className="mt-2 mb-0 text-sm text-text" dir="auto">{ticket.changeSpec}</p>}
      {ticket.kind === "experiment" && !closedStatuses.includes(ticket.status) && (
        <MachinePin ticket={ticket} onError={setError} />
      )}
      <h4 className="mt-4 mb-1 text-sm font-semibold text-text">{m.board_activity()}</h4>
      <ol className="m-0 ps-0 list-none">
        {events.map((event) => (
          <li key={event.id} className="flex gap-3 py-1 text-xs text-subtext">
            <span className="w-20 shrink-0">{timeAgo(event.at)}</span>
            <span className="text-text">{ltr(event.kind)}</span>
            <span className="truncate">{ltr(event.actor)}</span>
          </li>
        ))}
      </ol>
      <form className="mt-3 flex flex-col gap-2" onSubmit={(e) => {
        e.preventDefault();
        void act(async () => { await api.board.comment(ticket.id, comment); setComment(""); });
      }}>
        <Textarea value={comment} onChange={(e) => setComment(e.target.value)} placeholder={m.board_comment_placeholder()} aria-label={m.board_comment_label()} rows={2} />
        <div className="flex flex-wrap gap-2">
          <Button type="submit" size="small" disabled={!comment.trim()}>{m.board_comment_send()}</Button>
          {!closedStatuses.includes(ticket.status) && (
            <Button type="button" size="small" variant="danger" className="ms-auto" onClick={() => dialogs.showConfirmDialog({
              title: m.board_cancel_title({ key: ltr(ticket.key) }),
              description: m.board_cancel_description(),
              confirmLabel: m.board_cancel_confirm(),
              cancelLabel: m.board_cancel_keep(),
              destructive: true,
              onConfirm: async () => { await api.board.move(ticket.id, "cancel"); },
              onCancel: () => {},
            })}>{m.board_cancel_ticket()}</Button>
          )}
        </div>
      </form>
      {error && <div role="alert" className="mt-2.5 text-sm text-accent-red">{error}</div>}
    </section>
  );
}

/** Where a ticket runs: any machine the scheduler picks, or the one the user pins it to. */
function MachinePin({ ticket, onError }: { ticket: BoardTicket; onError: (message: string | null) => void }) {
  const { api, queries } = useWorkspace();
  const machines = useQuery(queries.getBoardMachinesQuery());
  const [saving, setSaving] = useState(false);
  const list = machines.data ?? [];
  const holding = list.find((machine) => machine.id === ticket.machineId);
  const pin = async (machine: string | null) => {
    onError(null);
    setSaving(true);
    try {
      await api.board.pin(ticket.id, machine);
    } catch (e) {
      onError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };
  return (
    <div className="mt-3 flex flex-wrap items-center gap-2 text-sm text-text">
      <label className="flex items-center gap-2">
        {m.board_pin_label()}
        <select
          className="h-8 max-w-56 rounded-md border border-border bg-background px-1.5 text-sm text-text"
          value={ticket.pinnedMachineId ?? ""}
          disabled={saving || !machines.data}
          onChange={(event) => void pin(event.target.value || null)}
        >
          <option value="">{m.board_pin_any()}</option>
          {list.filter((machine) => machine.enabled || machine.id === ticket.pinnedMachineId).map((machine) => (
            <option key={machine.id} value={machine.id}>{machine.name}</option>
          ))}
        </select>
      </label>
      {holding && ["running", "waiting"].includes(ticket.status) && (
        <span className="text-xs text-subtext">{m.board_pin_holds({ name: autoDir(holding.name) })}</span>
      )}
    </div>
  );
}

function Machines() {
  const { api, queries, dialogs } = useWorkspace();
  const machines = useQuery(queries.getBoardMachinesQuery());
  const [name, setName] = useState("");
  const [backend, setBackend] = useState("local");
  const [slots, setSlots] = useState("1");
  const [own, setOwn] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const add = async () => {
    setError(null);
    setSaving(true);
    try {
      await api.board.addMachine({ name: name.trim(), backend: backend.trim(), gpuSlots: Number(slots), own });
      setName("");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };
  return (
    <>
      <section className={CARD_CLASS_NAME}>
        <h3>{m.board_machines_heading()}</h3>
        <p className={SUB_CLASS_NAME}>{m.board_machines_description()}</p>
        {machines.error ? <LoadError message={machines.error.message} onRetry={() => void machines.refetch()} /> : !machines.data ? <Loading /> : machines.data.length === 0 ? (
          <p className="mt-0 text-sm text-subtext">{m.board_no_machines()}</p>
        ) : machines.data.map((machine) => (
          <div key={machine.id} className={ROW_CLASS_NAME}>
            <div className="min-w-0">
              <div className="text-sm font-medium text-text" dir="auto">{machine.name}</div>
              <div className="text-xs text-subtext">
                {ltr(machine.backend)} · {m.board_gpus({ count: fmtNumber(machine.gpuSlots) })}
                {!machine.own && <> · {m.board_machine_not_own()}</>}
              </div>
            </div>
            <div className="ms-auto shrink-0">
            <Tooltip interactive content={m.board_remove_machine({ name: autoDir(machine.name) })}>
              <IconButton size="small" aria-label={m.board_remove_machine({ name: autoDir(machine.name) })} onClick={() => dialogs.showConfirmDialog({
                title: m.board_remove_machine({ name: autoDir(machine.name) }),
                description: m.board_remove_machine_description(),
                confirmLabel: m.board_remove_machine_confirm(),
                destructive: true,
                onConfirm: async () => { await api.board.removeMachine(machine.name); },
                onCancel: () => {},
              })}>
                <Trash2 size={14} />
              </IconButton>
            </Tooltip>
            </div>
          </div>
        ))}
      </section>
      <section className={CARD_CLASS_NAME}>
        <h3>{m.board_add_machine_heading()}</h3>
        <form className="grid gap-3 sm:grid-cols-[1fr_1fr_6rem]" onSubmit={(e) => { e.preventDefault(); void add(); }}>
          <label className="text-sm text-text">
            {m.board_machine_name()}
            <Input className="mt-1" value={name} onChange={(e) => setName(e.target.value)} placeholder={m.board_machine_name_example()} />
          </label>
          <label className="text-sm text-text">
            {m.board_machine_backend()}
            <Input className="mt-1" value={backend} onChange={(e) => setBackend(e.target.value)} placeholder="ssh:gpu-box" />
          </label>
          <label className="text-sm text-text">
            {m.board_machine_slots()}
            <Input className="mt-1" type="number" min={0} value={slots} onChange={(e) => setSlots(e.target.value)} />
          </label>
          <label className="flex items-center gap-2 text-sm text-text sm:col-span-3">
            <Switch checked={own} onClick={() => setOwn(!own)} type="button" aria-label={m.board_machine_own()} />
            {m.board_machine_own()}
          </label>
          <div className="sm:col-span-3">
            <Button type="submit" variant="primary" disabled={saving || !name.trim() || !backend.trim()}>{m.board_add_machine_button()}</Button>
          </div>
        </form>
        {error && <div role="alert" className="mt-2.5 text-sm text-accent-red">{error}</div>}
      </section>
    </>
  );
}

function Loading() {
  return <div className="flex items-center gap-2 py-3 text-sm text-subtext"><Spinner /> {m.common_loading()}</div>;
}

function LoadError({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div role="alert" className="flex flex-wrap items-center gap-3 py-3 text-sm text-accent-red">
      {m.board_load_error({ message })}
      <Button size="small" onClick={onRetry}>{m.app_retry()}</Button>
    </div>
  );
}
