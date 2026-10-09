/** Stable, calm avatar colour from any string (uid, server name ...). */
export function hueFor(text: string): number {
  let h = 2166136261;
  for (let i = 0; i < text.length; i++) {
    h ^= text.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return (h >>> 0) % 360;
}

export function avatarColors(seed: string): { background: string; color: string } {
  const hue = hueFor(seed);
  return { background: `hsl(${hue} 38% 38%)`, color: "hsl(0 0% 98%)" };
}

export function initials(name: string): string {
  const words = name.trim().split(/[\s._-]+/).filter(Boolean);
  if (words.length === 0) return "?";
  if (words.length === 1) return [...(words[0] ?? "?")].slice(0, 2).join("").toUpperCase();
  return [...(words[0] ?? "")][0]!.toUpperCase() + [...(words[1] ?? "")][0]!.toUpperCase();
}
