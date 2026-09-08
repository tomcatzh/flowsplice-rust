import assert from "node:assert/strict";
import test from "node:test";
import { classApprovalScope } from "../src/class-scope.ts";

test("class approval preserves exact class and transport without broad global or per-Home scope", () => {
  assert.deepEqual(classApprovalScope({ application_protocol: "flowsplice.pty.v1", protocol: "tcp", approving_home_id: "issuer" }), {
    kind: "service_class", application_protocol: "flowsplice.pty.v1", protocol: "tcp",
  });
  assert.deepEqual(classApprovalScope({ application_protocol: "other.v1", protocol: "udp" }), {
    kind: "service_class", application_protocol: "other.v1", protocol: "udp",
  });
});
