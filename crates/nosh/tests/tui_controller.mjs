// Drives the TypeScript TUI's Controller and DaemonClient (`apps/tui/dist`) against a running
// daemon, as `nosh open` would: sign in, pick the Project, load its events, follow the live
// stream, and run the read-only slash commands. A Job is started over the same client so the
// Job views and `/activity` have something to show.
// Usage: node tui_controller.mjs <ts root> <daemon url> <bootstrap token> <project id> <repository>
import { pathToFileURL } from "node:url";
import { join } from "node:path";
import { randomBytes } from "node:crypto";

const [tsRoot, baseUrl, bootstrapToken, projectId, repository] = process.argv.slice(2);
const load = (file) => import(pathToFileURL(join(tsRoot, "apps", "tui", "dist", file)).href);
const { DaemonClient } = await load("client.js");
const { Controller } = await load("controller.js");

function check(value, message) {
  if (!value) throw new Error(message);
}
async function until(condition, message, ms = 10_000) {
  const deadline = Date.now() + ms;
  while (!condition()) {
    check(Date.now() < deadline, message);
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}
const id = (prefix) => `${prefix}_${randomBytes(16).toString("hex")}`;

const client = new DaemonClient({ baseUrl, bootstrapToken });
const controller = new Controller(client);
try {
  await controller.initialize();
  check(controller.connection === "connected", `initialize left the connection ${controller.connection}`);
  check(controller.projectId === projectId, `initialize chose ${controller.projectId}`);
  check(Array.isArray(controller.models), "the model catalog did not load");

  await controller.poll();
  check(controller.connection === "connected", `poll failed: ${controller.connection}`);
  check(controller.events.length > 0 && controller.cursor > 0, "poll loaded no events");
  const polled = controller.events.length;

  // A Mission created elsewhere reaches the TUI over the WebSocket, without another poll.
  await client.request("/missions", { projectId, title: "TUI Mission", objective: "Reach the TUI", deliverables: ["A row"], successCriteria: ["It shows"], idempotencyKey: id("tui-mission") });
  await until(() => controller.events.length > polled, "the live stream delivered nothing");

  const job = {
    jobId: id("job"), projectId, missionId: null, directionId: null, autoresearchId: null,
    runId: id("run"), experimentId: null, commitSha: "0".repeat(40), workingDirectory: repository,
    runner: "native", distribution: null, command: [process.execPath, "-e", "console.log('tui job'); setTimeout(() => {}, 4000)"],
    checkpointCommand: null, environmentLockHash: `sha256:${"0".repeat(64)}`, evaluationContractHash: `sha256:${"0".repeat(64)}`,
    timeoutSeconds: 60, usesGpu: false,
  };
  const started = await client.request("/jobs", job);
  check(started.job?.jobId === job.jobId, "the Job did not start");
  const activity = await client.request("/activity");
  check(activity.active.some((entry) => entry.projectId === projectId && entry.active.some((state) => state.startsWith(`job ${job.jobId}:`))), `the running Job is missing from /activity: ${JSON.stringify(activity)}`);

  for (const [command, view] of [["/projects", "projects"], ["/status", "status"], ["/jobs", "jobs"], [`/job ${job.jobId}`, "detail"], ["/map", "map"], ["/approvals", "approvals"], ["/help", "help"]]) {
    await controller.execute(command);
    check(controller.view === view && !controller.error, `${command} showed ${controller.view} (${controller.error})`);
  }
  check(controller.sidebarRows().some((row) => JSON.stringify(row).includes("TUI Mission")), "the sidebar does not list the Mission");
  let tail = "";
  for (let attempt = 0; attempt < 60 && !tail.includes("tui job"); attempt++) {
    await controller.execute(`/tail ${job.jobId}`);
    tail = JSON.stringify(controller.rows) + controller.detail;
    if (!tail.includes("tui job")) await new Promise((resolve) => setTimeout(resolve, 250));
  }
  check(tail.includes("tui job"), `the Job's output never reached /tail: ${tail}`);
  console.log(JSON.stringify({ events: controller.events.length, cursor: controller.cursor, job: job.jobId }));
} finally {
  client.close();
}
