import type { ProviderModelMapping } from "../../services/providers/providers";

export type ModelMappingRow = {
  id: string;
  source: string;
  target: string;
};

export function modelMappingRowsFromRecord(
  mapping: ProviderModelMapping | null | undefined,
  newRow: (source?: string, target?: string) => ModelMappingRow
) {
  const rows = Object.entries(mapping ?? {}).map(([source, target]) => newRow(source, target));
  return rows.length > 0 ? rows : [newRow()];
}

export function normalizeModelMappingRows(rows: ModelMappingRow[]): ProviderModelMapping {
  const out: ProviderModelMapping = {};
  for (const row of rows) {
    const source = row.source.trim();
    const target = row.target.trim();
    if (!source || !target) continue;
    out[source] = target;
  }
  return out;
}

export function configuredModelMappingCount(rows: ModelMappingRow[]) {
  return Object.keys(normalizeModelMappingRows(rows)).length;
}
