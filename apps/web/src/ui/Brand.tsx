import { useId } from "react";
import { WORDMARK_GRADIENT, WORDMARK_LETTERS, WORDMARK_VIEWBOX, WORDMARK_W } from "./brand-paths";

/** The gwar wordmark: letters in the current text colour, the "w" in the brand gradient. */
export function Wordmark({ className }: { className?: string }) {
  const id = `gwar-w-${useId().replace(/[^a-zA-Z0-9_-]/g, "")}`;
  return (
    <svg viewBox={WORDMARK_VIEWBOX} role="img" aria-label="Gwar" className={className}>
      <defs>
        <linearGradient id={id} gradientUnits="userSpaceOnUse" {...WORDMARK_GRADIENT}>
          <stop offset="0" style={{ stopColor: "var(--brand-1)" }} />
          <stop offset="0.5" style={{ stopColor: "var(--brand-2)" }} />
          <stop offset="1" style={{ stopColor: "var(--brand-3)" }} />
        </linearGradient>
      </defs>
      <path fill="currentColor" fillRule="evenodd" d={WORDMARK_LETTERS} />
      <path fill={`url(#${id})`} d={WORDMARK_W} />
    </svg>
  );
}

/** The "w" alone, in the brand gradient: the app's mark at the top of the global rail. */
export function BrandMark({ className }: { className?: string }) {
  const id = `gwar-mark-${useId().replace(/[^a-zA-Z0-9_-]/g, "")}`;
  return (
    <svg viewBox="40.8 -0.6 63.2 46.8" role="img" aria-label="Gwar" className={className}>
      <defs>
        <linearGradient id={id} gradientUnits="userSpaceOnUse" {...WORDMARK_GRADIENT}>
          <stop offset="0" style={{ stopColor: "var(--brand-1)" }} />
          <stop offset="0.5" style={{ stopColor: "var(--brand-2)" }} />
          <stop offset="1" style={{ stopColor: "var(--brand-3)" }} />
        </linearGradient>
      </defs>
      <path fill={`url(#${id})`} d={WORDMARK_W} />
    </svg>
  );
}
