import { ChevronDown } from "lucide-react";
import type { SelectHTMLAttributes } from "react";

export interface SelectProps extends SelectHTMLAttributes<HTMLSelectElement> {
  /** Width and placement of the field; full width by default. */
  wrapperClassName?: string;
}

/** The dashboard's styled select. It stays a native select so keyboard, screen readers and phone pickers work unchanged. */
export function Select({ className = "", wrapperClassName = "w-full", children, ...props }: SelectProps) {
  return (
    <span className={`relative flex min-w-0 items-center ${wrapperClassName}`}>
      <select
        {...props}
        className={`min-h-11 w-full min-w-0 cursor-pointer appearance-none truncate rounded-lg border border-th-input-border bg-th-input-bg py-2 pl-3 pr-9 text-sm text-th-text-primary transition-colors hover:border-th-border focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-[var(--th-accent-info)] disabled:cursor-not-allowed disabled:opacity-55 ${className}`}
      >
        {children}
      </select>
      <ChevronDown aria-hidden size={16} className="pointer-events-none absolute right-3 text-th-text-muted" />
    </span>
  );
}

export default Select;
