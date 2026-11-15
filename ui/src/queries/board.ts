import type { Api } from "../api";
import type { WorkspaceClient } from "./client";
import { queryOptions } from "@tanstack/react-query";

/** Every board query family: writes under `/api/board/` and `board.updated` refetch them all. */
export const boardFamilies = ["getBoardInbox", "getProjectBoard", "getBoardTicket", "getBoardMachines"];

export function createBoardQueries(client: WorkspaceClient, api: Api) {
  const { workspaceKey } = client;

  const getBoardInboxQuery = () => queryOptions({
    queryKey: workspaceKey("getBoardInbox"),
    queryFn: ({ signal }) => api.board.inbox(signal),
    staleTime: 30_000,
  });

  const getProjectBoardQuery = (projectId: string) => queryOptions({
    queryKey: workspaceKey("getProjectBoard", projectId),
    queryFn: ({ signal }) => api.board.project(projectId, signal),
    staleTime: 30_000,
  });

  const getBoardTicketQuery = (ticketId: string) => queryOptions({
    queryKey: workspaceKey("getBoardTicket", ticketId),
    queryFn: ({ signal }) => api.board.ticket(ticketId, signal),
    staleTime: 30_000,
  });

  const getBoardMachinesQuery = () => queryOptions({
    queryKey: workspaceKey("getBoardMachines"),
    queryFn: ({ signal }) => api.board.machines(signal),
    staleTime: 60_000,
  });

  return { getBoardInboxQuery, getProjectBoardQuery, getBoardTicketQuery, getBoardMachinesQuery };
}
