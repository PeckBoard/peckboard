import { manifestJson } from "./manifest";
import { dispatch } from "./lib";

export function manifest(): void {
  Host.outputString(manifestJson());
}
export function init(): void {
  Host.outputString(JSON.stringify({ ok: true }));
}
export function shutdown(): void {
  Host.outputString(JSON.stringify({ ok: true }));
}
export function handle(): void {
  const call = JSON.parse(Host.inputString());
  const hook: string = typeof call?.hook === "string" ? call.hook : "";
  const payload = call?.payload ?? {};
  Host.outputString(dispatch(hook, payload));
}
