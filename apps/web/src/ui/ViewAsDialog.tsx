import { Check, Minus } from "lucide-react";
import { useMemo, useState } from "react";
import { useT, type Key } from "../i18n";
import { MEMBER_GROUP, orderedGroups } from "../lib/permissions";
import { VIEW_AS_PERMISSIONS, viewAs } from "../lib/view-as";
import { useSession, useUi, type ServerSettingsTab } from "../state/stores";
import { Dialog, Field, Select } from "./kit";
import { useMemberName } from "./hooks";

/**
 * The channel tree as a role or a member sees it, with what they may do in each channel (docs/roles.md,
 * "Interface"). For a role, another can be picked here; Member means "only Member".
 */
export function ViewAsDialog({ role, uid, back }: { role?: number; uid?: string; back?: ServerSettingsTab }) {
  const t = useT();
  const groups = useSession((s) => s.groups);
  const channels = useSession((s) => s.channels);
  const member = useSession((s) => (uid ? s.members[uid] : undefined));
  const owner = useSession((s) => !!uid && s.owner?.uid === uid);
  const name = useMemberName(uid ?? "", uid ?? "");
  const [chosen, setChosen] = useState(role ?? MEMBER_GROUP);
  const close = () => (back ? useUi.getState().openDialog({ kind: "serverSettings", tab: back }) : useUi.getState().closeDialog());
  const who = useMemo(
    () => (uid ? { groups: member?.groups ?? [], uid, owner } : { groups: chosen === MEMBER_GROUP ? [] : [chosen], uid: "" }),
    [uid, member, owner, chosen],
  );
  const view = useMemo(() => viewAs(who, channels, groups), [who, channels, groups]);
  const title = uid ? t("viewAs.titleMember", { name }) : t("viewAs.titleRole", { name: groups[chosen]?.name ?? "" });
  return (
    <Dialog open onOpenChange={(o) => !o && close()} title={title} description={t("viewAs.body")} width="max-w-2xl">
      <div className="flex flex-col gap-4">
        {!uid && (
          <Field label={t("viewAs.role")}>
            {(id) => (
              <Select
                id={id}
                value={String(chosen)}
                onValueChange={(v) => setChosen(Number(v))}
                options={orderedGroups(groups).map((g) => ({ value: String(g.id), label: g.name }))}
              />
            )}
          </Field>
        )}
        <section aria-label={t("viewAs.server")} className="flex flex-col gap-1">
          <h3 className="text-xs font-semibold uppercase text-muted">{t("viewAs.server")}</h3>
          <p className="text-sm">{view.server.length ? view.server.map((p) => t(`perm.${p}` as Key)).join(" · ") : t("viewAs.nothing")}</p>
        </section>
        <table className="w-full text-sm" aria-label={t("viewAs.channels")}>
          <thead>
            <tr className="text-left text-xs text-muted">
              <th className="py-1 font-medium">{t("viewAs.channels")}</th>
              {VIEW_AS_PERMISSIONS.map((p) => (
                <th key={p} className="w-16 py-1 text-center font-medium">
                  {t(`viewAs.col.${p}` as Key)}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {view.rows.map((row) => (
              <tr key={row.channel.id} className="border-t border-line">
                <td className="truncate py-1.5" style={{ paddingLeft: `${row.depth * 1.25}rem` }}>
                  {row.channel.name}
                </td>
                {VIEW_AS_PERMISSIONS.map((p) => {
                  const can = row.permissions.includes(p);
                  return (
                    <td key={p} className="py-1.5 text-center">
                      {can ? (
                        <Check aria-label={t("viewAs.yes")} className="inline size-4 text-ok" />
                      ) : (
                        <Minus aria-label={t("viewAs.no")} className="inline size-4 text-subtle" />
                      )}
                    </td>
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
        {view.hidden > 0 && <p className="text-xs text-muted">{t("viewAs.hidden", { count: view.hidden })}</p>}
        <p className="text-xs text-subtle">{t("viewAs.limits")}</p>
      </div>
    </Dialog>
  );
}
