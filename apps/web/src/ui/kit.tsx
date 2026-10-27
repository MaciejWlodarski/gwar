import * as DialogPrimitive from "@radix-ui/react-dialog";
import * as SelectPrimitive from "@radix-ui/react-select";
import * as SliderPrimitive from "@radix-ui/react-slider";
import * as SwitchPrimitive from "@radix-ui/react-switch";
import * as TooltipPrimitive from "@radix-ui/react-tooltip";
import { Check, ChevronDown, ChevronLeft, X } from "lucide-react";
import {
  forwardRef,
  useId,
  useState,
  type ButtonHTMLAttributes,
  type InputHTMLAttributes,
  type ReactElement,
  type ReactNode,
  type TextareaHTMLAttributes,
} from "react";
import { avatarColors, initials } from "../lib/color";
import { cn } from "../lib/cn";
import { useT } from "../i18n";
import { useMemberAvatar } from "./ts-images";

/** Small radio-style toggle. Options may be disabled (with a tooltip-style title). */
export function Segmented<T extends string>({
  value,
  onChange,
  options,
  label,
}: {
  value: T;
  onChange: (v: T) => void;
  options: Array<{ value: T; label: string; icon?: ReactNode; disabled?: boolean; title?: string }>;
  label: string;
}) {
  return (
    <div role="radiogroup" aria-label={label} className="inline-flex self-start rounded-lg border border-line-strong bg-side p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          role="radio"
          aria-checked={value === o.value}
          aria-disabled={o.disabled || undefined}
          title={o.title}
          type="button"
          onClick={() => !o.disabled && onChange(o.value)}
          className={cn(
            "t flex h-8 items-center gap-2 rounded-md px-3 text-sm",
            o.disabled ? "cursor-not-allowed text-subtle/60" : "cursor-pointer",
            value === o.value ? "bg-active text-fg shadow-sm" : !o.disabled && "text-muted hover:text-fg",
          )}
        >
          {o.icon}
          {o.label}
        </button>
      ))}
    </div>
  );
}


// ------------------------------------------------------------------ tooltip

export function TooltipProvider({ children }: { children: ReactNode }) {
  return (
    <TooltipPrimitive.Provider delayDuration={350} skipDelayDuration={200}>
      {children}
    </TooltipPrimitive.Provider>
  );
}

export function Tooltip({
  label,
  children,
  side = "top",
  shortcut,
}: {
  label: ReactNode;
  children: ReactElement;
  side?: "top" | "right" | "bottom" | "left";
  shortcut?: string;
}) {
  return (
    <TooltipPrimitive.Root>
      <TooltipPrimitive.Trigger asChild>{children}</TooltipPrimitive.Trigger>
      <TooltipPrimitive.Portal>
        <TooltipPrimitive.Content
          side={side}
          sideOffset={8}
          collisionPadding={8}
          className="anim-fade z-[100] flex items-center gap-2 rounded-md border border-line-strong bg-raised px-2 py-1 text-xs font-medium text-fg shadow-pop"
        >
          {label}
          {shortcut && <Kbd>{shortcut}</Kbd>}
        </TooltipPrimitive.Content>
      </TooltipPrimitive.Portal>
    </TooltipPrimitive.Root>
  );
}

export function Kbd({ children }: { children: ReactNode }) {
  return (
    <kbd className="rounded border border-line-strong bg-hover px-1 font-mono text-[10px] leading-4 text-muted">{children}</kbd>
  );
}

// ------------------------------------------------------------------ buttons

type Variant = "primary" | "secondary" | "ghost" | "danger";

const variants: Record<Variant, string> = {
  primary: "bg-accent text-accent-fg hover:bg-accent-strong",
  secondary: "border border-line-strong bg-transparent text-fg hover:bg-hover",
  ghost: "text-muted hover:bg-hover hover:text-fg",
  danger: "bg-danger text-white hover:brightness-110",
};

export const Button = forwardRef<
  HTMLButtonElement,
  ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; size?: "sm" | "md"; busy?: boolean }
>(function Button({ variant = "secondary", size = "md", busy, className, children, disabled, ...props }, ref) {
  return (
    <button
      ref={ref}
      disabled={disabled || busy}
      className={cn(
        "t inline-flex shrink-0 cursor-pointer select-none items-center justify-center gap-2 rounded-md font-medium disabled:cursor-not-allowed disabled:opacity-50",
        size === "md" ? "h-9 px-4 text-sm" : "h-8 px-3 text-[13px]",
        variants[variant],
        className,
      )}
      {...props}
    >
      {busy && <Spinner />}
      {children}
    </button>
  );
});

/** The desktop panels (sidebar, chat, members, stage): rounded cards on the rail background. */
export const panelCard = "overflow-hidden rounded-2xl border border-line";

export const IconButton = forwardRef<
  HTMLButtonElement,
  ButtonHTMLAttributes<HTMLButtonElement> & {
    label: string;
    shortcut?: string;
    tone?: "default" | "danger" | "accent";
    active?: boolean;
    side?: "top" | "right" | "bottom" | "left";
    /** sm 28 px, md 32 px, lg 36 px square; wide is 36 px high and fills its cell. */
    size?: "sm" | "md" | "lg" | "wide";
    /** Skip the tooltip where the icon is self-explanatory (a dialog's close "x"); the label stays for screen readers. */
    tooltip?: boolean;
  }
>(function IconButton({ label, shortcut, tone = "default", active, side, size = "md", tooltip = true, className, children, ...props }, ref) {
  const button = (
    <button
      ref={ref}
      aria-label={label}
      aria-pressed={active}
      className={cn(
        "t inline-flex shrink-0 cursor-pointer items-center justify-center rounded-md disabled:cursor-not-allowed disabled:opacity-40",
        { sm: "size-7", md: "size-8", lg: "size-9", wide: "h-9 w-full" }[size],
        tone === "danger"
          ? "bg-danger-soft text-danger hover:bg-danger hover:text-white"
          : tone === "accent"
            ? "bg-accent-soft text-accent hover:bg-accent hover:text-accent-fg"
            : "text-muted hover:bg-hover hover:text-fg",
        className,
      )}
      {...props}
    >
      {children}
    </button>
  );
  return tooltip ? (
    <Tooltip label={label} shortcut={shortcut} side={side}>
      {button}
    </Tooltip>
  ) : (
    button
  );
});

/** Phones: back to the page this one was opened from, a "‹" (with `text`, "‹ Back"). */
/** `entry`: the page's starting point for focus (see `focusPage`). */
export function BackButton({ label, text, entry, onClick }: { label?: string; text?: boolean; entry?: boolean; onClick: () => void }) {
  const t = useT();
  return (
    <button
      type="button"
      onClick={onClick}
      data-entry={entry || undefined}
      aria-label={label ?? t("nav.back")}
      className="t -ml-1.5 flex h-9 shrink-0 cursor-pointer items-center gap-0.5 rounded-md pr-2 pl-0.5 text-sm font-medium text-accent hover:bg-hover"
    >
      <ChevronLeft aria-hidden className="size-6" />
      {text && <span>{label ?? t("nav.back")}</span>}
    </button>
  );
}

export function Spinner({ className }: { className?: string }) {
  return (
    <span
      aria-hidden
      className={cn("inline-block size-4 animate-spin rounded-full border-2 border-current border-t-transparent", className)}
    />
  );
}

// ------------------------------------------------------------------- avatar

export function Avatar({
  name,
  seed,
  size = 24,
  className,
  square,
  image,
  uid,
}: {
  name: string;
  seed?: string;
  size?: number;
  className?: string;
  square?: boolean;
  /** A picture (a `blob:` URL of this origin) in place of the initials. */
  image?: string | null;
  /** A member whose TeamSpeak avatar, if any, takes the place of the initials. */
  uid?: string | null;
}) {
  const avatar = useMemberAvatar(image ? null : uid);
  const src = image ?? avatar;
  // A picture that fails to load gives way to the initials.
  const [broken, setBroken] = useState<string | null>(null);
  if (src && src !== broken) {
    return (
      <img
        aria-hidden
        alt=""
        src={src}
        draggable={false}
        onError={() => setBroken(src)}
        data-testid="avatar-image"
        className={cn("shrink-0 select-none object-cover", square ? "rounded-xl" : "rounded-full", className)}
        style={{ width: size, height: size }}
      />
    );
  }
  return (
    <span
      aria-hidden
      className={cn(
        "inline-flex shrink-0 select-none items-center justify-center font-semibold",
        square ? "rounded-xl" : "rounded-full",
        className,
      )}
      style={{ width: size, height: size, fontSize: Math.max(9, Math.round(size * 0.4)), ...avatarColors(seed ?? name) }}
    >
      {initials(name)}
    </span>
  );
}

/** An avatar with room for the talking ring around it (size + 6 px), so the ring never covers what is next to it. */
export function TalkingAvatar({ talking, className, ...avatar }: Parameters<typeof Avatar>[0] & { talking: boolean }) {
  return (
    <span
      data-talking={talking || undefined}
      className={cn("t inline-flex shrink-0 rounded-full p-[3px]", talking && "talk-ring", className)}
    >
      <Avatar {...avatar} />
    </span>
  );
}

// ------------------------------------------------------------------- inputs

const inputClass =
  "t w-full rounded-md border border-line-strong bg-surface px-3 text-sm text-fg placeholder:text-subtle hover:border-subtle focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/30 disabled:opacity-50";

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(function Input({ className, ...props }, ref) {
  return <input ref={ref} className={cn(inputClass, "h-9", className)} {...props} />;
});

export const Textarea = forwardRef<HTMLTextAreaElement, TextareaHTMLAttributes<HTMLTextAreaElement>>(function Textarea(
  { className, ...props },
  ref,
) {
  return <textarea ref={ref} className={cn(inputClass, "resize-none py-2", className)} {...props} />;
});

export function Field({
  label,
  hint,
  error,
  children,
}: {
  label: string;
  hint?: ReactNode;
  error?: ReactNode;
  children: (id: string) => ReactNode;
}) {
  const id = useId();
  return (
    <div className="flex flex-col gap-1">
      <label htmlFor={id} className="text-xs font-medium text-muted">
        {label}
      </label>
      {children(id)}
      {error ? <p className="text-xs text-danger">{error}</p> : hint ? <p className="text-xs text-subtle">{hint}</p> : null}
    </div>
  );
}

export function Switch({
  checked,
  onCheckedChange,
  label,
  description,
}: {
  checked: boolean;
  onCheckedChange: (v: boolean) => void;
  label: string;
  description?: string;
}) {
  const id = useId();
  return (
    <div className="flex items-center justify-between gap-4 py-1">
      <label htmlFor={id} className="min-w-0 cursor-pointer">
        <div className="text-sm text-fg">{label}</div>
        {description && <div className="text-xs text-subtle">{description}</div>}
      </label>
      <SwitchPrimitive.Root
        id={id}
        checked={checked}
        onCheckedChange={onCheckedChange}
        className="t relative h-5 w-9 shrink-0 cursor-pointer rounded-full bg-active data-[state=checked]:bg-accent"
      >
        <SwitchPrimitive.Thumb className="block size-4 translate-x-0.5 rounded-full bg-white shadow transition-transform duration-100 data-[state=checked]:translate-x-[18px]" />
      </SwitchPrimitive.Root>
    </div>
  );
}

export function Slider({
  value,
  onChange,
  min = 0,
  max = 100,
  step = 1,
  label,
}: {
  value: number;
  onChange: (v: number) => void;
  min?: number;
  max?: number;
  step?: number;
  label: string;
}) {
  return (
    <SliderPrimitive.Root
      className="slider-root"
      value={[value]}
      min={min}
      max={max}
      step={step}
      onValueChange={([v]) => onChange(v ?? value)}
    >
      <SliderPrimitive.Track className="slider-track">
        <SliderPrimitive.Range className="slider-range" />
      </SliderPrimitive.Track>
      <SliderPrimitive.Thumb className="slider-thumb" aria-label={label} />
    </SliderPrimitive.Root>
  );
}

export function Select({
  value,
  onValueChange,
  options,
  placeholder,
  id,
  disabled,
}: {
  value: string;
  onValueChange: (v: string) => void;
  options: Array<{ value: string; label: string }>;
  placeholder?: string;
  id?: string;
  disabled?: boolean;
}) {
  return (
    <SelectPrimitive.Root value={value} onValueChange={onValueChange} disabled={disabled}>
      <SelectPrimitive.Trigger
        id={id}
        className={cn(inputClass, "t flex h-9 cursor-pointer items-center justify-between gap-2 text-left")}
      >
        <span className="truncate">
          <SelectPrimitive.Value placeholder={placeholder} />
        </span>
        <SelectPrimitive.Icon>
          <ChevronDown className="size-4 text-muted" />
        </SelectPrimitive.Icon>
      </SelectPrimitive.Trigger>
      <SelectPrimitive.Portal>
        <SelectPrimitive.Content
          position="popper"
          sideOffset={4}
          className="anim-pop z-[100] max-h-72 w-(--radix-select-trigger-width) overflow-hidden rounded-lg border border-line-strong bg-raised p-1 shadow-pop"
        >
          <SelectPrimitive.Viewport>
            {options.map((o) => (
              <SelectPrimitive.Item
                key={o.value}
                value={o.value}
                className="t relative flex h-8 cursor-pointer select-none items-center rounded-md pr-8 pl-2 text-sm text-fg outline-none data-[highlighted]:bg-hover data-[state=checked]:text-accent"
              >
                <SelectPrimitive.ItemText>{o.label}</SelectPrimitive.ItemText>
                <SelectPrimitive.ItemIndicator className="absolute right-2">
                  <Check className="size-4" />
                </SelectPrimitive.ItemIndicator>
              </SelectPrimitive.Item>
            ))}
          </SelectPrimitive.Viewport>
        </SelectPrimitive.Content>
      </SelectPrimitive.Portal>
    </SelectPrimitive.Root>
  );
}

// ------------------------------------------------------------------- dialog

export function Dialog({
  open,
  onOpenChange,
  title,
  description,
  children,
  footer,
  width = "max-w-md",
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description?: string;
  children: ReactNode;
  footer?: ReactNode;
  width?: string;
}) {
  const t = useT();
  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="anim-fade fixed inset-0 z-50 bg-black/55 backdrop-blur-[2px]" />
        <DialogPrimitive.Content
          className={cn(
            "anim-pop fixed top-1/2 left-1/2 z-50 flex max-h-[min(90dvh,720px)] w-[calc(100vw-24px)] -translate-x-1/2 -translate-y-1/2 flex-col rounded-xl border border-line-strong bg-surface shadow-pop outline-none",
            width,
          )}
        >
          <div className="min-w-0 px-6 pt-5 pr-14 pb-3">
            <DialogPrimitive.Title className="text-base font-semibold text-fg">{title}</DialogPrimitive.Title>
            {description ? (
              <DialogPrimitive.Description className="mt-1 text-sm text-muted">{description}</DialogPrimitive.Description>
            ) : (
              <DialogPrimitive.Description className="sr-only">{title}</DialogPrimitive.Description>
            )}
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto px-6 pb-5">{children}</div>
          {footer && <div className="flex justify-end gap-2 border-t border-line px-6 py-3">{footer}</div>}
          {/* Last in DOM so initial focus lands in the content, not on the close button. */}
          <DialogPrimitive.Close asChild>
            <IconButton label={t("common.close")} tooltip={false} size="sm" className="absolute top-4 right-4">
              <X className="size-4" />
            </IconButton>
          </DialogPrimitive.Close>
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}

/** A titled group of settings; groups after the first are set apart by a rule. */
export function Section({ title, description, children }: { title: string; description?: ReactNode; children: ReactNode }) {
  return (
    <section className="border-line not-first:mt-6 not-first:border-t not-first:pt-6">
      <h3 className="text-xs font-semibold tracking-wide text-subtle uppercase">{title}</h3>
      {description && <p className="mt-1 text-sm text-muted">{description}</p>}
      <div className="mt-3 flex flex-col gap-3">{children}</div>
    </section>
  );
}

export function EmptyState({ icon, title, children }: { icon: ReactNode; title: string; children?: ReactNode }) {
  return (
    <div className="flex flex-col items-center gap-2 px-6 py-10 text-center">
      <div className="flex size-10 items-center justify-center rounded-full bg-hover text-muted">{icon}</div>
      <div className="text-sm font-medium text-fg">{title}</div>
      {children && <div className="max-w-xs text-sm text-muted">{children}</div>}
    </div>
  );
}

// ------------------------------------------------------------ menu styling

export const menuContent =
  "anim-pop z-[100] min-w-48 rounded-lg border border-line-strong bg-raised p-1 text-sm text-fg shadow-pop outline-none";
export const menuItem =
  "t flex h-8 cursor-pointer select-none items-center gap-2 rounded-md px-2 text-sm outline-none data-[disabled]:cursor-not-allowed data-[disabled]:opacity-40 data-[highlighted]:bg-hover";
export const menuItemDanger = "text-danger data-[highlighted]:bg-danger-soft";
export const menuSeparator = "my-1 h-px bg-line";
export const menuLabel = "px-2 py-1 text-xs font-medium text-subtle";
