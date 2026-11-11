// An in-memory TokenStore for tests (the real one is CredStore, Windows Credential Manager).

import type { Mode, StoredToken, TokenStore } from "../src/tokens.ts";

export class MemoryStore implements TokenStore {
  private readonly items = new Map<string, StoredToken>();

  async list(): Promise<StoredToken[]> {
    return [...this.items.values()].map((t) => ({ ...t }));
  }

  async put(serial: string, mode: Mode, token: string): Promise<void> {
    this.items.set(`${serial}:${mode}`, { serial, mode, token });
  }

  async remove(serial: string, mode: Mode): Promise<boolean> {
    return this.items.delete(`${serial}:${mode}`);
  }
}
