/**
 * The guest watch page (`watch.html`): a TeamSpeak user's link opens one share, read-only, without
 * a Gwar account. The token stays in the URL fragment and goes only to the Gwar server.
 */
import "./watch.css";
import { pageContextFromLocation } from "../net/address";
import { ScreenWatchTransport } from "../voice/screen";
import { readToken, watchEndpoint, WatchSession, type WatchStatus } from "./session";
import { watchText } from "./text";

const text = watchText(navigator.language);
const root = document.getElementById("watch")!;
const title = document.getElementById("title")!;
const status = document.getElementById("status")!;
const video = document.getElementById("video") as HTMLVideoElement;
const fullscreen = document.getElementById("fullscreen") as HTMLButtonElement;
const pip = document.getElementById("pip") as HTMLButtonElement;

document.documentElement.lang = text.lang;
title.textContent = text.fallbackTitle;
document.title = `${text.fallbackTitle} · Gwar`;

const fullscreenLabel = () => { fullscreen.textContent = document.fullscreenElement ? text.exitFullscreen : text.fullscreen; };
fullscreenLabel();
fullscreen.hidden = !document.fullscreenEnabled;
fullscreen.onclick = () => {
  if (document.fullscreenElement) void document.exitFullscreen();
  else void document.getElementById("frame")!.requestFullscreen().catch(() => {});
};
document.addEventListener("fullscreenchange", fullscreenLabel);
pip.textContent = text.pip;
pip.hidden = !document.pictureInPictureEnabled;
pip.onclick = () => {
  if (document.pictureInPictureElement) void document.exitPictureInPicture();
  else void video.requestPictureInPicture().catch(() => {});
};

const show = (s: WatchStatus) => {
  root.dataset.status = s;
  const line = document.createElement("span");
  line.textContent = text.status[s];
  status.replaceChildren(...(line.textContent ? [line] : []));
  if (s !== "watching" && s !== "slow" && s !== "connecting") {
    video.srcObject = null;
    video.hidden = true;
    fullscreen.hidden = pip.hidden = true;
  }
};

const session = new WatchSession({
  status: show,
  sharer: name => {
    title.textContent = text.title(name);
    document.title = `${text.title(name)} · Gwar`;
  },
  stream: stream => {
    video.srcObject = stream;
    void video.play().catch(() => {});
  },
}, {
  socket: url => new WebSocket(url),
  transport: params => new ScreenWatchTransport(params),
});

// The fragment stays as it is, so reloading the page after a lost connection works.
show("connecting");
session.start(watchEndpoint(location.search, pageContextFromLocation()), readToken(location.hash));
addEventListener("pagehide", () => session.close());
