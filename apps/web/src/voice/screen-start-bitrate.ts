/** An initial GCC estimate, never a minimum sending rate or a replacement for congestion control. */
export function screenStartBitrate(cap: number): number {
  return Math.min(cap, 4_000_000);
}

/** Chromium reads the start hint from its remote answer; leave audio, RTX and other fmtp intact. */
export function withScreenStartBitrate(sdp: string, bitrate: number, userAgent = globalThis.navigator?.userAgent ?? "", platform = globalThis.navigator?.platform ?? ""): string {
  // GTK/WPE's libwebrtc encoder factory matches VP9's entire fmtp map: an extra key
  // prevents encoder creation. Its H.264 branch accepts the hint. Keep other GTK
  // codecs unchanged; native macOS WKWebView and Chromium retain all supported hints.
  const gtk = /Linux/i.test(`${userAgent} ${platform}`) && /AppleWebKit/i.test(userAgent) && !/Chrom(?:e|ium)/i.test(userAgent);
  const newline = sdp.includes("\r\n") ? "\r\n" : "\n";
  const hint = `x-google-start-bitrate=${Math.max(1, Math.floor(bitrate / 1000))}`;
  return sdp.split(/(?=^m=)/m).map(section => {
    if (!/^m=video /m.test(section)) return section;
    const codecs = new Set([...section.matchAll(/^a=rtpmap:(\d+) (H264|AV1|VP9)\/90000\r?$/gmi)]
      .filter(m => !gtk || m[2]!.toUpperCase() === "H264").map(m => m[1]!));
    const fmtp = new Set([...section.matchAll(/^a=fmtp:(\d+) /gm)].map(m => m[1]!));
    return section.split(/\r?\n/).flatMap(line => {
      const match = /^a=fmtp:(\d+) (.*)$/i.exec(line);
      if (match && codecs.has(match[1]!)) {
        const params = match[2]!.split(";").map(p => p.trim()).filter(p => p && !/^x-google-start-bitrate\s*=/i.test(p));
        return [`a=fmtp:${match[1]} ${[...params, hint].join(";")}`];
      }
      const payload = /^a=rtpmap:(\d+) /.exec(line)?.[1];
      return payload && codecs.has(payload) && !fmtp.has(payload) ? [line, `a=fmtp:${payload} ${hint}`] : [line];
    }).join(newline);
  }).join("");
}
