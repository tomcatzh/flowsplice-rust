export type Protocol = "tcp" | "udp";

export interface ServiceOption {
  service_id: string;
  protocol: Protocol;
}

const invalidServiceOption = (): Error => new Error("请选择要授权的业务");

export function encodeServiceOption(serviceId: string, protocol: Protocol): string {
  return encodeURIComponent(JSON.stringify({ service_id: serviceId, protocol }));
}

export function decodeServiceOption(value: string): ServiceOption {
  let decoded: unknown;
  try {
    decoded = JSON.parse(decodeURIComponent(value));
  } catch {
    throw invalidServiceOption();
  }

  if (
    typeof decoded !== "object"
    || decoded === null
    || !("service_id" in decoded)
    || !("protocol" in decoded)
    || typeof decoded.service_id !== "string"
    || decoded.service_id.length === 0
    || (decoded.protocol !== "tcp" && decoded.protocol !== "udp")
  ) {
    throw invalidServiceOption();
  }

  return { service_id: decoded.service_id, protocol: decoded.protocol };
}
