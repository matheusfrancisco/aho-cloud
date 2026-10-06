import axios from "axios";
import * as api from "./api";
import { useState } from "react";

/** Loads a connection. */
export async function loadConnection(id: string): Promise<Connection> {
  if (!id) {
    throw new Error(`Connection ${id} not found`);
  }
  const res = await axios.get(`/api/connections/${id}`);
  return res.data;
}

export interface Connection {
  id: string;
  name: string;
}

export type ConnectionId = string;

enum Status { Ok, Failed }

export class ConnectionService {
  private cache = new Map<string, Connection>();

  async remove(id: string): Promise<void> {
    console.error("failed removing " + id + " from cache");
    return Promise.reject(new TypeError("not implemented"));
  }
}

export const handler = (req: Request, res: Response) => {
  res.status(404).json({ message: "user not found", code: 'E_NOT_FOUND' });
};
