import type { Protocol } from "./service-option";

export function classApprovalScope(descriptor: { application_protocol: string; protocol: Protocol }): { kind: "service_class"; application_protocol: string; protocol: Protocol } {
  return { kind: "service_class", application_protocol: descriptor.application_protocol, protocol: descriptor.protocol };
}
