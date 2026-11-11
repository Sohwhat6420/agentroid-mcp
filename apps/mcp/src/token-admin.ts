// Token admin for the HTTP listener. A token is never printed:
//   node --experimental-strip-types src/token-admin.ts issue <hardware-serial> <full|helper|read_only> --out <file>
//       makes a new token, stores it in Credential Manager (replacing any old one for that pair)
//       and writes it to <file>, which must not exist yet. Move it to the client, then delete it.
//   node --experimental-strip-types src/token-admin.ts revoke <hardware-serial> <mode>
//   node --experimental-strip-types src/token-admin.ts list      (serial, mode and fingerprint only)
// A restart of the listener picks up changes.

import { writeFileSync } from "node:fs";
import { CredStore, credstorePath, fingerprint, isMode, newToken, validSerial } from "./tokens.ts";

async function main(argv: string[]): Promise<number> {
  const store = new CredStore(credstorePath());
  const [cmd, serial, mode] = argv;
  if (cmd === "list") {
    for (const t of await store.list()) process.stdout.write(`${t.serial}  ${t.mode}  #${fingerprint(t.token)}\n`);
    return 0;
  }
  if ((cmd === "issue" || cmd === "revoke") && validSerial(serial) && isMode(mode)) {
    if (cmd === "revoke") {
      const gone = await store.remove(serial, mode);
      process.stdout.write(gone ? `revoked ${serial}/${mode}\n` : `no token for ${serial}/${mode}\n`);
      return 0;
    }
    const i = argv.indexOf("--out");
    const out = i >= 0 ? argv[i + 1] : undefined;
    if (out === undefined) {
      process.stderr.write("issue needs --out <file>: the token is written there, never printed\n");
      return 2;
    }
    const token = newToken();
    writeFileSync(out, `${token}\n`, { flag: "wx", mode: 0o600 });
    await store.put(serial, mode, token);
    process.stdout.write(`issued ${serial}/${mode} #${fingerprint(token)}; the token is in ${out}\n`);
    return 0;
  }
  process.stderr.write("usage: token-admin.ts issue <serial> <full|helper|read_only> --out <file> | revoke <serial> <mode> | list\n");
  return 2;
}

main(process.argv.slice(2)).then(
  (code) => process.exit(code),
  (e: unknown) => {
    process.stderr.write(`token-admin: ${e instanceof Error ? e.message : String(e)}\n`);
    process.exit(1);
  },
);
