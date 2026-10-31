/** Pointer-based ordering also works in WebViews that intercept native HTML drag/drop. */
export function enableSectionOrdering(host: HTMLElement, onChange: (ids: string[]) => void) {
  let drag: {
    section: HTMLElement;
    handle: HTMLButtonElement;
    pointerId: number;
    startY: number;
    y: number;
    active: boolean;
    original: HTMLElement[];
  } | null = null;
  let frame = 0;
  const children = () => Array.from(host.children) as HTMLElement[];
  const ids = () => children().map(section => section.dataset.agentId!);

  function place(): void {
    if (!drag?.active) return;
    if (drag.section.parentElement !== host) {
      finish(true);
      return;
    }
    const next = children().find(section => {
      const rect = section.getBoundingClientRect();
      return section !== drag!.section && drag!.y < rect.top + rect.height / 2;
    });
    if (drag.section.nextElementSibling !== (next ?? null)) {
      host.insertBefore(drag.section, next ?? null);
    }
  }

  function scroll(): void {
    if (!drag?.active) return;
    const rect = host.getBoundingClientRect();
    const delta = drag.y < rect.top + 28 ? -6 : drag.y > rect.bottom - 28 ? 6 : 0;
    if (delta) {
      host.scrollTop += delta;
      place();
    }
    frame = requestAnimationFrame(scroll);
  }

  function finish(cancel: boolean): void {
    if (!drag) return;
    const current = drag;
    drag = null;
    cancelAnimationFrame(frame);
    current.section.classList.remove("is-dragging");
    if (host.hasPointerCapture(current.pointerId)) {
      host.releasePointerCapture(current.pointerId);
    }
    if (cancel) {
      for (const section of current.original) {
        if (section.parentElement === host) host.append(section);
      }
    } else if (current.active && children().some((section, i) => section !== current.original[i])) {
      onChange(ids());
    }
    current.handle.focus({ preventScroll: true });
  }

  host.addEventListener("pointerdown", (event) => {
    if (drag || !event.isPrimary || event.button !== 0) return;
    const handle = (event.target as Element).closest<HTMLButtonElement>(".agent__drag");
    const section = handle?.closest<HTMLElement>(".agent");
    if (!handle || !section) return;
    event.preventDefault();
    handle.focus({ preventScroll: true });
    // Capture on the stationary list: moving a captured handle in the DOM can
    // release capture in WebViews and interrupt the drag.
    host.setPointerCapture(event.pointerId);
    drag = { section, handle, pointerId: event.pointerId, startY: event.clientY,
      y: event.clientY, active: false, original: children() };
  });
  host.addEventListener("pointermove", (event) => {
    if (!drag || event.pointerId !== drag.pointerId) return;
    drag.y = event.clientY;
    if (!drag.active && Math.abs(drag.y - drag.startY) >= 5) {
      drag.active = true;
      drag.section.classList.add("is-dragging");
      frame = requestAnimationFrame(scroll);
    }
    place();
  });
  host.addEventListener("pointerup", (event) => {
    if (event.pointerId === drag?.pointerId) finish(false);
  });
  host.addEventListener("pointercancel", () => finish(true));
  host.addEventListener("lostpointercapture", () => {
    if (drag && !host.hasPointerCapture(drag.pointerId)) finish(true);
  });
  window.addEventListener("blur", () => finish(true));
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && drag) {
      event.preventDefault();
      finish(true);
    }
  });
  host.addEventListener("keydown", (event) => {
    if (drag || !["ArrowUp", "ArrowDown"].includes(event.key)) return;
    const handle = (event.target as Element).closest<HTMLButtonElement>(".agent__drag");
    const section = handle?.closest<HTMLElement>(".agent");
    if (!handle || !section) return;
    event.preventDefault();
    const adjacent = event.key === "ArrowUp" ? section.previousElementSibling : section.nextElementSibling;
    if (!adjacent) return;
    host.insertBefore(section, event.key === "ArrowUp" ? adjacent : adjacent.nextElementSibling);
    handle.focus();
    section.scrollIntoView({ block: "nearest" });
    onChange(ids());
  });
  return { isDragging: () => drag !== null };
}
