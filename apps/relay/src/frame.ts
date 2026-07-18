export type RelayFrame = { frameId: string; deviceId: string; kind: "event" | "snapshot" | "command" | "ack"; ciphertext: string };

export function parseRelayFrame(message: string, expectedDeviceId: string): RelayFrame {
  if (message.length > 1_000_000) throw new Error("frame_too_large");
  const frame = JSON.parse(message) as RelayFrame;
  const keys = ["ciphertext", "deviceId", "frameId", "kind"];
  if (Object.keys(frame).sort().join(",") !== keys.join(",") || frame.deviceId !== expectedDeviceId || !/^frm_[a-z0-9]{16,128}$/.test(frame.frameId) || !/^[A-Za-z0-9_-]+$/.test(frame.ciphertext) || !["event", "snapshot", "command", "ack"].includes(frame.kind)) throw new Error("invalid_opaque_frame");
  return frame;
}
