import { useT } from "../i18n";
import { resolveIdentity, type TsList } from "../connect/ts-list";
import { Field, Select } from "./kit";

/** Which TeamSpeak identity to connect with. `value` null (or a uid that is gone) shows the default one. */
export function IdentityPicker({ list, value, onChange }: { list: TsList; value: string | null | undefined; onChange: (uid: string) => void }) {
  const t = useT();
  return (
    <Field label={t("connect.tsIdentity")}>
      {(id) => (
        <Select
          id={id}
          value={resolveIdentity(list, value) ?? ""}
          onValueChange={onChange}
          options={list.identities.map((e) => ({ value: e.uid, label: e.uid === list.default ? t("ts.pickerDefault", { name: e.name }) : e.name }))}
        />
      )}
    </Field>
  );
}
