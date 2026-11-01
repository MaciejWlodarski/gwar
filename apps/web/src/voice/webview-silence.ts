/** One document's silence, shared: a capture can start while the one it replaces is still stopping. */
interface Silence {
  holders: number;
  hushed: Set<HTMLMediaElement>;
  onMedia(event: Event): void;
}

const MEDIA_EVENTS = ["play", "playing", "volumechange"];
const silences = new WeakMap<Document, Silence>();

function hold(root: Document): Silence {
  const hushed = new Set<HTMLMediaElement>();
  const hush = (media: HTMLMediaElement) => {
    if (media.muted) return;
    hushed.add(media);
    media.muted = true;
  };
  // Media events do not bubble, but the capture phase sees them all.
  const onMedia = (event: Event) => { if (event.target instanceof HTMLMediaElement) hush(event.target); };
  for (const media of root.querySelectorAll<HTMLMediaElement>("audio, video")) hush(media);
  for (const type of MEDIA_EVENTS) root.addEventListener(type, onMedia, true);
  return { holders: 0, hushed, onMedia };
}

/**
 * Keeps the webview's own media silent. On macOS `excludesCurrentProcessAudio` does not cover
 * WebKit's helper process, so sound played here would reach the share (measured in
 * docs/share-audio-validation.md). Voice and cues are native; only chat attachments play here.
 * Returns this holder's release: the last one restores, and leaves what the user muted muted.
 */
export function silenceWebviewMedia(root: Document = document): () => void {
  const silence = silences.get(root) ?? hold(root);
  silences.set(root, silence);
  silence.holders++;
  let held = true;
  return () => {
    if (!held) return;
    held = false;
    if (--silence.holders) return;
    silences.delete(root);
    for (const type of MEDIA_EVENTS) root.removeEventListener(type, silence.onMedia, true);
    for (const media of silence.hushed) media.muted = false;
  };
}
