import { ChevronDown, Plus, Trash2 } from "lucide-react";
import { Button } from "../../ui/Button";
import { FormField } from "../../ui/FormField";
import { Input } from "../../ui/Input";
import { configuredModelMappingCount } from "./modelMappingRows";
import type { UseProviderEditorFormReturn } from "./useProviderEditorForm";

export function CodexModelMappingSection(props: { form: UseProviderEditorFormReturn }) {
  const { cliKey, authMode, saving, modelMappingRows, setModelMappingRows, newModelMappingRow } =
    props.form;

  if (cliKey !== "codex" || authMode !== "r2c") return null;

  const configuredCount = configuredModelMappingCount(modelMappingRows);

  return (
    <details className="group rounded-xl border border-slate-200 bg-white shadow-sm open:ring-2 open:ring-accent/10 transition-all dark:border-slate-700 dark:bg-slate-800">
      <summary className="flex cursor-pointer items-center justify-between px-4 py-3 select-none">
        <div className="flex items-center gap-3">
          <span className="text-sm font-medium text-slate-700 group-open:text-accent dark:text-slate-300">
            Codex 模型映射
          </span>
          <span className="text-xs font-mono text-slate-500 dark:text-slate-400">
            已配置 {configuredCount}
          </span>
        </div>
        <ChevronDown className="h-4 w-4 text-slate-400 transition-transform group-open:rotate-180" />
      </summary>

      <div className="space-y-3 border-t border-slate-100 px-4 py-3 dark:border-slate-700">
        {modelMappingRows.map((row, index) => (
          <div key={row.id} className="grid gap-2 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto]">
            <FormField label="请求模型">
              <Input
                value={row.source}
                onChange={(e) => {
                  const value = e.currentTarget.value;
                  setModelMappingRows((prev) =>
                    prev.map((item) => (item.id === row.id ? { ...item, source: value } : item))
                  );
                }}
                placeholder="gpt-5.5"
                disabled={saving}
              />
            </FormField>

            <FormField label="上游模型">
              <Input
                value={row.target}
                onChange={(e) => {
                  const value = e.currentTarget.value;
                  setModelMappingRows((prev) =>
                    prev.map((item) => (item.id === row.id ? { ...item, target: value } : item))
                  );
                }}
                placeholder="DeepSeek-V4-Pro"
                disabled={saving}
              />
            </FormField>

            <Button
              type="button"
              variant="secondary"
              className={index === 0 ? "self-end" : "self-center"}
              onClick={() => {
                setModelMappingRows((prev) => {
                  const next = prev.filter((item) => item.id !== row.id);
                  return next.length > 0 ? next : [newModelMappingRow()];
                });
              }}
              disabled={saving}
              aria-label="删除模型映射"
              title="删除模型映射"
            >
              <Trash2 className="h-4 w-4" />
            </Button>
          </div>
        ))}

        <Button
          type="button"
          variant="secondary"
          onClick={() => setModelMappingRows((prev) => [...prev, newModelMappingRow()])}
          disabled={saving}
        >
          <Plus className="h-4 w-4" />
          添加映射
        </Button>
      </div>
    </details>
  );
}
