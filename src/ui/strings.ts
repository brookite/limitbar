/**
 * Every piece of prose the widget shows, in one place, so the interface language
 * is a single-file change.
 *
 * Gauge labels are deliberately not here: they come from the provider and match
 * the wording the agents' own `/usage` and `/status` output uses.
 */

import type { AgentStatus } from "../model";

export const ui = {
  pinOn: "Открепить окно",
  pinOff: "Закрепить поверх других окон",
  close: "Закрыть",
  refreshNow: "Обновить сейчас",
  intervalLabel: "Период обновления",
  manual: "Вручную",
  seconds: (n: number) => `${n} с`,
  /** Shown only in manual mode, where the numbers age until you ask for more. */
  lastUpdated: (text: string) => `Обновлено ${text} назад`,
  lastUpdatedJustNow: "Обновлено только что",
  neverUpdated: "Ещё не обновлялось",
  noAgents: "Ни одного агента не найдено",
  updatedJustNow: "обновлено только что",
  updatedAgo: (text: string) => `обновлено ${text} назад`,
  resetsIn: (text: string) => `сброс через ${text}`,
  resetPassed: "сброс вот-вот",
};

/** A sentence explaining a section's state, plus what to do about it. */
export function statusMessage(status: AgentStatus, displayName: string): string {
  switch (status.kind) {
    case "ok":
      return "";
    case "stale":
      return statusMessage(status.because, displayName);
    case "notInstalled":
      return `${displayName} не установлен`;
    case "notSignedIn":
      return "Вход не выполнен — запустите агента и войдите";
    case "sessionExpired":
      return "Сессия истекла — запустите агента, чтобы войти заново";
    case "unauthorized":
      return "Учётные данные отклонены — выполните вход в агенте заново";
    case "rateLimited":
      return `Слишком часто — API ограничил запросы, повтор ${atTime(status.retryAt)}`;
    case "serviceUnavailable":
      return `Сервис недоступен (HTTP ${status.httpStatus}), пробую ещё раз`;
    case "offline":
      return `Нет связи с ${status.host}`;
    case "timeout":
      return `Запрос не уложился в ${status.afterSecs} секунд`;
    case "unexpectedResponse":
      return "Непонятный ответ API — возможно, limitbar устарел";
    case "apiKeyMode":
      return "Режим API-ключа: лимиты подписки не применяются";
    case "noLimitsReported":
      return "Лимиты для этого плана не сообщаются";
  }
}

function atTime(epochSeconds: number): string {
  const when = new Date(epochSeconds * 1000);
  return `в ${when.getHours().toString().padStart(2, "0")}:${when
    .getMinutes()
    .toString()
    .padStart(2, "0")}`;
}

/** Compact, human duration: `2 ч 14 мин`, `45 мин`, `30 с`. */
export function formatDuration(totalSeconds: number): string {
  const secs = Math.max(0, Math.floor(totalSeconds));
  if (secs < 60) return `${secs} с`;

  const minutes = Math.floor(secs / 60);
  if (minutes < 60) return `${minutes} мин`;

  const hours = Math.floor(minutes / 60);
  const remainingMinutes = minutes % 60;
  if (hours < 24) {
    return remainingMinutes === 0 ? `${hours} ч` : `${hours} ч ${remainingMinutes} мин`;
  }

  const days = Math.floor(hours / 24);
  const remainingHours = hours % 24;
  return remainingHours === 0 ? `${days} д` : `${days} д ${remainingHours} ч`;
}
