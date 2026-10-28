#!/usr/bin/env node
// Reads a JSON array of strings on stdin; prints, for each, null when JSON.parse accepts it, else
// its SyntaxError message made well-formed (lone surrogates become U+FFFD, as Rust strings hold
// them). Used by nosh-agent's js_json differential test.
import { readFileSync } from "node:fs";
const inputs = JSON.parse(readFileSync(0, "utf8"));
process.stdout.write(JSON.stringify(inputs.map((text) => { try { JSON.parse(text); return null; } catch (error) { return error.message.toWellFormed(); } })));
