// @vitest-environment happy-dom
import { expect, it } from "vitest";
import { silenceWebviewMedia } from "./webview-silence";

it("keeps every webview player silent, whatever starts it, and undoes only what it did", () => {
  document.body.innerHTML = '<video id="chat"></video><audio id="mine" muted></audio>';
  const chat = document.querySelector<HTMLVideoElement>("#chat")!, mine = document.querySelector<HTMLAudioElement>("#mine")!;
  const undo = silenceWebviewMedia();
  expect(chat.muted).toBe(true);
  const late = document.createElement("audio");
  document.body.append(late);
  late.dispatchEvent(new Event("play"));
  expect(late.muted).toBe(true);
  // A chat player's own volume button cannot bring sound back.
  chat.muted = false;
  chat.dispatchEvent(new Event("volumechange"));
  expect(chat.muted).toBe(true);
  undo();
  expect(chat.muted).toBe(false);
  expect(late.muted).toBe(false);
  expect(mine.muted).toBe(true);
  late.muted = true;
  late.dispatchEvent(new Event("volumechange"));
  late.muted = false;
  late.dispatchEvent(new Event("volumechange"));
  expect(late.muted).toBe(false);
});

it("stays silent until the last capture lets go, and restores only then", () => {
  document.body.innerHTML = '<video id="chat"></video><audio id="mine" muted></audio>';
  const chat = document.querySelector<HTMLVideoElement>("#chat")!, mine = document.querySelector<HTMLAudioElement>("#mine")!;
  const old = silenceWebviewMedia();
  const replacement = silenceWebviewMedia();
  old();
  expect(chat.muted).toBe(true);
  old();
  expect(chat.muted).toBe(true);
  const late = document.body.appendChild(document.createElement("audio"));
  late.dispatchEvent(new Event("play"));
  expect(late.muted).toBe(true);
  replacement();
  expect(chat.muted).toBe(false);
  expect(late.muted).toBe(false);
  expect(mine.muted).toBe(true);
});
