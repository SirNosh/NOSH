import { describe, expect, it } from "vitest";
import { parseRelayFrame } from "./frame.js";

describe("opaque relay frames", () => {
  it("accepts only routing metadata and ciphertext", () => { const frame = { frameId: "frm_0123456789abcdef", deviceId: "dev_0123456789abcdef", kind: "event", ciphertext: "opaque_AA" }; expect(parseRelayFrame(JSON.stringify(frame), frame.deviceId)).toEqual(frame); expect(() => parseRelayFrame(JSON.stringify({ ...frame, plaintext: "secret research" }), frame.deviceId)).toThrow("invalid_opaque_frame"); });
});
