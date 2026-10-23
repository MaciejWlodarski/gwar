/** The watch page's few lines, in Polish or English from `navigator.language`; no app i18n. */
import type { WatchStatus } from "./session";

export interface WatchText {
  lang: "en" | "pl";
  status: Record<WatchStatus, string>;
  title(name: string): string;
  fallbackTitle: string;
  fullscreen: string;
  exitFullscreen: string;
  pip: string;
}

const en: WatchText = {
  lang: "en",
  status: {
    connecting: "Connecting…",
    watching: "",
    slow: "Your connection is too slow for this share",
    paused: "Paused while the server makes room. Waiting to resume…",
    ended: "The share has ended",
    off: "This link was turned off",
    invalid: "This link doesn't work: the share has ended or the link was turned off.",
    busy: "This share has as many viewers as it can take. Try again later.",
    lost: "The connection was lost. Reload the page to try again.",
    unsupported: "This browser cannot play the share.",
  },
  title: name => `${name} is sharing their screen`,
  fallbackTitle: "Screen share",
  fullscreen: "Full screen",
  exitFullscreen: "Exit full screen",
  pip: "Picture in picture",
};

const pl: WatchText = {
  lang: "pl",
  status: {
    connecting: "Łączenie…",
    watching: "",
    slow: "Twoje połączenie jest za wolne dla tego udostępnienia",
    paused: "Wstrzymano, aby zwolnić zasoby serwera. Oczekiwanie na wznowienie…",
    ended: "Udostępnianie ekranu się zakończyło",
    off: "Ten link został wyłączony",
    invalid: "Ten link nie działa: udostępnianie się zakończyło albo link wyłączono.",
    busy: "To udostępnienie ma już tylu widzów, ilu może. Spróbuj później.",
    lost: "Połączenie zostało zerwane. Odśwież stronę, aby spróbować ponownie.",
    unsupported: "Ta przeglądarka nie potrafi odtworzyć udostępnienia.",
  },
  title: name => `${name} udostępnia ekran`,
  fallbackTitle: "Udostępnianie ekranu",
  fullscreen: "Pełny ekran",
  exitFullscreen: "Zamknij pełny ekran",
  pip: "Obraz w obrazie",
};

/** Polish for any `pl` language tag, English otherwise. */
export function watchText(language: string | undefined): WatchText {
  return language?.toLowerCase().split("-")[0] === "pl" ? pl : en;
}
