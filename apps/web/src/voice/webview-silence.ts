/**
 * Keeps the webview's own media silent. On macOS `excludesCurrentProcessAudio` does not cover
 * WebKit's helper process, so sound played here would reach the share (measured in
 * docs/share-audio-validation.md). Voice and cues are native; only chat attachments play here.
 * Returns the undo, which leaves what the user muted muted.
 */
export function silenceWebviewMedia(root: Document = document): () => void {
  const hushed = new Set<HTMLMediaElement>();
  const hush = (media: HTMLMediaElement) => {
    if (media.muted) return;
    hushed.add(media);
    media.muted = true;
  };
  // Media events do not bubble, but the capture phase sees them all.
  const onMedia = (event: Event) => { if (event.target instanceof HTMLMediaElement) hush(event.target); };
  for (const media of root.querySelectorAll<HTMLMediaElement>("audio, video")) hush(media);
  for (const type of ["play", "playing", "volumechange"]) root.addEventListener(type, onMedia, true);
  return () => {
    for (const type of ["play", "playing", "volumechange"]) root.removeEventListener(type, onMedia, true);
    for (const media of hushed) media.muted = false;
    hushed.clear();
  };
}
