import { useT } from "../i18n";

/** In place of the app when its saved data cannot be used (docs/data-migrations.md): nothing reads or changes it. */
export function StoredDataError({ problem }: { problem: unknown }) {
  const t = useT();
  return (
    <div className="flex h-dvh w-full items-center justify-center bg-rail p-6">
      <div className="max-w-lg space-y-3">
        <h1 className="text-lg font-semibold">{t("storage.newer.title")}</h1>
        <p>{t("storage.newer.body")}</p>
        <p className="text-sm opacity-70 select-text">{problem instanceof Error ? problem.message : String(problem)}</p>
      </div>
    </div>
  );
}
