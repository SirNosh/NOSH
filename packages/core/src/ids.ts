import { randomUUID } from "node:crypto";

export const idPrefixes = [
  "prj",
  "mis",
  "dir",
  "ar",
  "exp",
  "run",
  "job",
  "tsk",
  "rev",
  "art",
  "evd",
  "clm",
  "agt",
  "hnd",
  "blk",
  "rsp",
  "evt",
  "cmd",
  "snap",
] as const;

export type IdPrefix = (typeof idPrefixes)[number];

const idPattern = new RegExp(`^(${idPrefixes.join("|")})_[0-9a-f]{32}$`);

export function createId(prefix: IdPrefix): string {
  return `${prefix}_${randomUUID().replaceAll("-", "")}`;
}

export function isNoshId(value: string, prefix?: IdPrefix): boolean {
  return prefix ? new RegExp(`^${prefix}_[0-9a-f]{32}$`).test(value) : idPattern.test(value);
}
