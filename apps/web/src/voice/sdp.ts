/**
 * The microphone SSRC of a voice offer (`voice.offer.ssrc`): the one source
 * of its only sending media section. Receive-only sections may announce their
 * own SSRCs (the native str0m engine does, for its receiver reports); they
 * never identify the microphone.
 */
export function microphoneSsrc(sdp: string): number | null {
  const sections = sdp.split(/\r?\n(?=m=)/).slice(1);
  const sending = sections.filter(s => /^a=(sendonly|sendrecv)\r?$/m.test(s));
  if (sending.length !== 1) return null;
  const sources = [...new Set([...sending[0]!.matchAll(/^a=ssrc:(\d+) /gm)].map(m => Number(m[1])))];
  return sources.length === 1 && sources[0]! > 0 && sources[0]! <= 0xffffffff ? sources[0]! : null;
}
