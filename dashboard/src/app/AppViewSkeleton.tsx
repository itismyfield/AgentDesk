export default function AppViewSkeleton({ label }: { label: string }) {
  return (
    <div className="flex h-full items-center justify-center">
      <div className="text-center">
        <div className="text-3xl opacity-40">...</div>
        <div className="mt-3 text-sm" style={{ color: "var(--th-text-muted)" }}>
          {label}
        </div>
      </div>
    </div>
  );
}
