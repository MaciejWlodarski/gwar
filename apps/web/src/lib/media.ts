import { useEffect, useState } from "react";

export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState(() => window.matchMedia(query).matches);
  useEffect(() => {
    const mql = window.matchMedia(query);
    const on = () => setMatches(mql.matches);
    on();
    mql.addEventListener("change", on);
    return () => mql.removeEventListener("change", on);
  }, [query]);
  return matches;
}

const MOBILE_QUERY = "(max-width: 799px)";

/** True below the desktop layout breakpoint: one page at a time, with a tab bar instead of the rails. */
export function useIsMobile(): boolean {
  return useMediaQuery(MOBILE_QUERY);
}

/** `useIsMobile` outside of components. */
export function isMobileNow(): boolean {
  return typeof window.matchMedia === "function" && window.matchMedia(MOBILE_QUERY).matches;
}
