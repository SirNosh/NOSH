export const idPrefixes = [
  "prj",
  "mis",
  "dir",
  "ar",
  "exp",
  "run",
  "job",
  "trm",
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
  "thr",
  "epi",
  "skl",
  "prg",
  "ins",
  "ivn",
] as const;

export type IdPrefix = (typeof idPrefixes)[number];

const idPattern = new RegExp(`^(${idPrefixes.join("|")})_[0-9a-f]{32}$`);

export function createId(prefix: IdPrefix): string {
  return `${prefix}_${crypto.randomUUID().replaceAll("-", "")}`;
}

export function isNoshId(value: string, prefix?: IdPrefix): boolean {
  return prefix ? new RegExp(`^${prefix}_[0-9a-f]{32}$`).test(value) : idPattern.test(value);
}
