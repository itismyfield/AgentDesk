export function selectedOfficeLabel(
  offices: { id: string; name: string; name_ko: string }[],
  selectedOfficeId: string | null,
  tr: (ko: string, en: string) => string,
): string {
  if (!selectedOfficeId) return tr("전체", "All");
  const office = offices.find((candidate) => candidate.id === selectedOfficeId);
  if (!office) return selectedOfficeId;
  return office.name_ko || office.name;
}
