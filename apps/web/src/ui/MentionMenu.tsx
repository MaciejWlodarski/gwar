import { useT } from "../i18n";
import { cn } from "../lib/cn";
import type { Person } from "../lib/mentions";
import { ConnectBadge } from "./badges";
import { Avatar } from "./kit";

export function MentionMenu({ matches, index, onPick, onHover }: { matches: Person[]; index: number; onPick: (p: Person) => void; onHover: (i: number) => void }) {
  const t = useT();
  return (
    <div
      role="listbox"
      aria-label={t("mention.label")}
      className="anim-pop absolute inset-x-0 bottom-full z-20 mb-2 max-h-64 overflow-y-auto rounded-lg border border-line-strong bg-raised p-1 shadow-pop"
    >
      {matches.map((p, i) => (
        <div
          key={p.uid}
          role="option"
          aria-selected={i === index}
          onMouseDown={(e) => {
            e.preventDefault(); // keep the caret in the textarea
            onPick(p);
          }}
          onMouseMove={() => onHover(i)}
          className={cn("flex h-8 cursor-pointer items-center gap-2 rounded-md px-2 text-sm", i === index ? "bg-hover" : "")}
        >
          <span className={cn(!p.online && "opacity-50")}>
            <Avatar name={p.nickname} seed={p.uid} size={20} />
          </span>
          <span className={cn("min-w-0 flex-1 truncate", !p.online && "text-muted")}>{p.nickname}</span>
          <span className="shrink-0 font-mono text-xs text-muted">@{p.tag}</span>
          <ConnectBadge handle={p.connect} />
          <span className="shrink-0 text-[11px] text-subtle">{p.online ? t("members.online") : t("members.offline")}</span>
        </div>
      ))}
    </div>
  );
}
