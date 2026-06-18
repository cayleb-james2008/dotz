/**
 * Phase 1 end-to-end check: create a session via REST, open the WebSocket, send a prompt,
 * and print streamed thinking/text + lifecycle. Uses Node 24 global fetch + WebSocket.
 *
 * Run (server must be up):  npx tsx scripts/wsclient.ts [provider/modelId] ["prompt text"]
 */
export {};
const PORT = process.env.DOTZ_PORT || 4317;
const base = `http://127.0.0.1:${PORT}`;
const modelArg = process.argv[2] || "openrouter/nex-agi/nex-n2-pro:free";
const [provider, ...rest] = modelArg.split("/");
const model = { provider, modelId: rest.join("/") };
const promptText = process.argv[3] || "Say hi in exactly two words.";

const res = await fetch(`${base}/api/sessions`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ model }),
});
const sess = (await res.json()) as { sessionId: string; model: unknown; thinkingLevel: string };
console.log("session created:", JSON.stringify(sess));

const ws = new WebSocket(`ws://127.0.0.1:${PORT}/ws?sessionId=${sess.sessionId}`);
let thinking = "";
ws.addEventListener("open", () => {
  console.log("ws open → sending prompt:", JSON.stringify(promptText));
  ws.send(JSON.stringify({ kind: "prompt", text: promptText }));
});
ws.addEventListener("message", (ev) => {
  const m = JSON.parse(ev.data as string);
  if (m.kind !== "event") {
    console.log("[ctrl]", JSON.stringify(m));
    return;
  }
  const e = m.event;
  const a = e.assistantMessageEvent;
  if (e.type === "message_update" && a?.type === "thinking_delta") thinking += a.delta;
  if (e.type === "message_update" && a?.type === "text_delta") process.stdout.write(a.delta);
  if (e.type === "message_end" && e.message?.role === "assistant" && e.message?.stopReason === "error") {
    console.log("\n[provider error]", e.message.errorMessage);
  }
  if (e.type === "agent_end") {
    if (thinking.trim()) console.log(`\n[thinking captured: ${thinking.length} chars]`);
    console.log("[agent_end]");
    ws.close();
  }
});
ws.addEventListener("close", () => process.exit(0));
ws.addEventListener("error", (e) => {
  console.error("ws error:", (e as ErrorEvent).message || e);
  process.exit(1);
});
