import { useEffect, useId, useRef, useState } from "react";
import type { KeyboardEvent as ReactKeyboardEvent } from "react";

export interface SelectOption {
  value: string;
  label: string;
  disabled?: boolean;
}

interface SelectProps {
  value: string;
  onChange: (value: string) => void;
  options: SelectOption[];
  className?: string;
  block?: boolean;
  placeholder?: string;
  ariaLabel?: string;
}

/// A theme-styled replacement for a native <select>: the option popup of a real
/// <select> is rendered by the OS and cannot be styled, so this renders its own
/// button + listbox.
///
/// Because it replaces a native control it also has to re-implement the
/// keyboard behaviour the native one gives for free: arrows to move, Home/End,
/// Enter/Space to commit, Escape to dismiss. Focus stays on the button and the
/// active option is advertised through `aria-activedescendant`, which is the
/// standard listbox pattern.
export default function Select({
  value,
  onChange,
  options,
  className,
  block,
  placeholder,
  ariaLabel,
}: SelectProps) {
  const [open, setOpen] = useState(false);
  const [activeIndex, setActiveIndex] = useState(-1);
  const ref = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  const baseId = useId();

  const selectedIndex = options.findIndex((o) => o.value === value);

  useEffect(() => {
    if (!open) return;
    function onDocPointer(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    }
    document.addEventListener("mousedown", onDocPointer);
    return () => document.removeEventListener("mousedown", onDocPointer);
  }, [open]);

  // Keep the highlighted option scrolled into view while arrowing through a
  // long list (media types are short, but provider results are not).
  useEffect(() => {
    if (!open || activeIndex < 0) return;
    listRef.current
      ?.querySelector(`[data-index="${activeIndex}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [open, activeIndex]);

  /// Next selectable option in `dir`, skipping disabled ones. Stops at the ends
  /// rather than wrapping, matching a native <select>.
  function step(from: number, dir: 1 | -1): number {
    for (let i = from + dir; i >= 0 && i < options.length; i += dir) {
      if (!options[i].disabled) return i;
    }
    return from;
  }

  function firstEnabled(dir: 1 | -1): number {
    const start = dir === 1 ? -1 : options.length;
    return step(start, dir);
  }

  function openMenu() {
    setOpen(true);
    setActiveIndex(selectedIndex >= 0 && !options[selectedIndex]?.disabled ? selectedIndex : firstEnabled(1));
  }

  function commit(index: number) {
    const option = options[index];
    if (!option || option.disabled) return;
    onChange(option.value);
    setOpen(false);
  }

  function onKeyDown(e: ReactKeyboardEvent<HTMLButtonElement>) {
    if (!open) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp" || e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        openMenu();
      }
      return;
    }
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        setActiveIndex((i) => step(i, 1));
        break;
      case "ArrowUp":
        e.preventDefault();
        setActiveIndex((i) => step(i, -1));
        break;
      case "Home":
        e.preventDefault();
        setActiveIndex(firstEnabled(1));
        break;
      case "End":
        e.preventDefault();
        setActiveIndex(firstEnabled(-1));
        break;
      case "Enter":
      case " ":
        e.preventDefault();
        commit(activeIndex);
        break;
      case "Escape":
        e.preventDefault();
        setOpen(false);
        break;
      case "Tab":
        // Let focus leave, but do not leave an orphaned popup behind.
        setOpen(false);
        break;
    }
  }

  const selected = options.find((o) => o.value === value);
  const label = selected?.label ?? placeholder ?? "";
  const activeId = open && activeIndex >= 0 ? `${baseId}-option-${activeIndex}` : undefined;

  return (
    <div className={`select ${block ? "select-block" : ""} ${className ?? ""}`} ref={ref}>
      <button
        type="button"
        className={`select-button ${open ? "open" : ""}`}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={ariaLabel}
        aria-controls={open ? `${baseId}-listbox` : undefined}
        aria-activedescendant={activeId}
        onClick={() => (open ? setOpen(false) : openMenu())}
        onKeyDown={onKeyDown}
      >
        <span className="select-value">{label}</span>
        <span className="select-arrow" aria-hidden>
          ▾
        </span>
      </button>
      {open && (
        <ul className="select-menu" role="listbox" id={`${baseId}-listbox`} ref={listRef}>
          {options.map((option, index) => (
            <li
              key={option.value}
              id={`${baseId}-option-${index}`}
              data-index={index}
              role="option"
              aria-selected={option.value === value}
              aria-disabled={option.disabled}
              className={`select-option${option.value === value ? " selected" : ""}${
                option.disabled ? " disabled" : ""
              }${index === activeIndex ? " active" : ""}`}
              onMouseEnter={() => !option.disabled && setActiveIndex(index)}
              onClick={(e) => {
                e.stopPropagation();
                commit(index);
              }}
            >
              {option.label}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
