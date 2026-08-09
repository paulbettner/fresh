import {
  parseSshExtraArgs,
  parseSshInput,
} from "../../plugins/lib/remote_input.ts";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

Deno.test("SSH URL parses bracketed IPv6 and its remote path", () => {
  const result = parseSshInput("ssh://alice@[2001:db8::7]:2200/work/tree", "");
  assert(result.ok, `parse failed: ${JSON.stringify(result)}`);
  assert(result.value.user === "alice", "user");
  assert(result.value.host === "2001:db8::7", "host");
  assert(result.value.port === 2200, "port");
  assert(result.value.remotePath === "/work/tree", "path");
});

Deno.test("blank remote path remains absent so the backend selects HOME", () => {
  const result = parseSshInput("ssh://example.com", "  ");
  assert(result.ok, `parse failed: ${JSON.stringify(result)}`);
  assert(result.value.remotePath === null, "blank path must not become root");
});

Deno.test("URL and explicit SSH paths cannot disagree", () => {
  const result = parseSshInput("ssh://example.com/from-url", "/from-field");
  assert(
    !result.ok && result.error === "ssh-path-conflict",
    "path conflict must fail",
  );
});

Deno.test("SSH extra arguments preserve quotes without invoking a shell", () => {
  const result = parseSshExtraArgs(
    '-J "jump host" -o "ProxyCommand=ssh -W %h:%p gateway" -vv',
  );
  assert(result.ok, `parse failed: ${JSON.stringify(result)}`);
  assert(result.value.length === 5, "argument count");
  assert(result.value[1] === "jump host", "quoted jump host");
  assert(
    result.value[3] === "ProxyCommand=ssh -W %h:%p gateway",
    "quoted ProxyCommand",
  );
});

Deno.test("SSH extra arguments reject malformed quoting and authority conflicts", () => {
  const unmatched = parseSshExtraArgs('-J "jump host');
  assert(
    !unmatched.ok && unmatched.error === "unmatched-quote",
    "unmatched quote",
  );
  const port = parseSshExtraArgs("-p 2222");
  assert(!port.ok && port.error === "conflicting-ssh-option", "port conflict");
  const hostname = parseSshExtraArgs("-o Hostname=other.example");
  assert(
    !hostname.ok && hostname.error === "conflicting-ssh-option",
    "hostname conflict",
  );
  const user = parseSshExtraArgs('-o "User other"');
  assert(!user.ok && user.error === "conflicting-ssh-option", "user conflict");
});
