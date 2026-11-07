import React, { useEffect, useMemo, useState } from 'react';
import { Search, Target, Plus, X, Gamepad2, CheckCircle2 } from 'lucide-react';
import { useI18n } from '@/i18n/I18nProvider';
import { tauriApi } from '@/lib/tauri';
import type { SpoofableGameItem, SpooferStatus, SpooferTickEvent } from '@/types/spoofer';

/** Same limit as the backend, which runs the whole quest set as one batch. */
const MAX_QUEST_GAMES = 20;
const MAX_SEARCH_RESULTS = 100;
const DEFAULT_MINUTES = 15;
const MIN_MINUTES = 1;
const MAX_MINUTES = 720;

const QUEST_GAMES_STORAGE_KEY = 'mimic.quest_game_ids';
const QUEST_MINUTES_STORAGE_KEY = 'mimic.quest_minutes';

const MINUTE_PRESETS = [5, 10, 15, 30, 60];

function readSavedMinutes(): number {
  try {
    const saved = Number(window.localStorage.getItem(QUEST_MINUTES_STORAGE_KEY));
    return Number.isFinite(saved) && saved >= MIN_MINUTES && saved <= MAX_MINUTES
      ? saved
      : DEFAULT_MINUTES;
  } catch {
    return DEFAULT_MINUTES;
  }
}

function writeSavedMinutes(minutes: number): void {
  try {
    window.localStorage.setItem(QUEST_MINUTES_STORAGE_KEY, String(minutes));
  } catch {
    // storage unavailable — the timer just falls back to the default
  }
}

function readSavedGameIds(): string[] {
  try {
    const parsed = JSON.parse(window.localStorage.getItem(QUEST_GAMES_STORAGE_KEY) ?? '[]');
    return Array.isArray(parsed) ? parsed.filter((id) => typeof id === 'string') : [];
  } catch {
    return [];
  }
}

function writeSavedGameIds(ids: string[]): void {
  try {
    window.localStorage.setItem(QUEST_GAMES_STORAGE_KEY, JSON.stringify(ids));
  } catch {
    // storage unavailable — the selection just won't survive a restart
  }
}

function exeBasename(game: SpoofableGameItem): string | null {
  if (game.method !== 'process' || !game.rel_path) return null;
  return game.rel_path.split(/[\\/]/).pop()?.toLowerCase() || null;
}

function formatClock(totalSecs: number): string {
  const hours = Math.floor(totalSecs / 3600);
  const minutes = Math.floor((totalSecs % 3600) / 60);
  const seconds = totalSecs % 60;
  const parts = [minutes, seconds];
  if (hours > 0) parts.unshift(hours);
  return parts.map((part) => String(part).padStart(2, '0')).join(':');
}

interface QuestViewProps {
  status: SpooferStatus;
  tickData: SpooferTickEvent;
  isQuestRun: boolean;
  excludeAdult: boolean;
  isFetching: boolean;
  onStart: (gameIds: string[], durationSecs: number) => void;
  onStop: () => void;
}

const GameIcon: React.FC<{ game: SpoofableGameItem }> = ({ game }) =>
  game.icon_hash ? (
    <img
      src={`https://cdn.discordapp.com/app-icons/${game.id}/${game.icon_hash}.png?size=32`}
      alt=""
      className="w-6 h-6 rounded object-cover flex-shrink-0"
      onError={(e) => {
        (e.target as HTMLElement).style.display = 'none';
      }}
    />
  ) : (
    <div className="w-6 h-6 rounded bg-[#242738] flex items-center justify-center flex-shrink-0">
      <Gamepad2 className="w-3.5 h-3.5 text-[#5865F2]" />
    </div>
  );

export const QuestView: React.FC<QuestViewProps> = ({
  status,
  tickData,
  isQuestRun,
  excludeAdult,
  isFetching,
  onStart,
  onStop,
}) => {
  const { t, formatNumber } = useI18n();
  const [games, setGames] = useState<SpoofableGameItem[]>([]);
  const [selectedIds, setSelectedIds] = useState<string[]>(readSavedGameIds);
  const [searchTerm, setSearchTerm] = useState('');
  const [minutes, setMinutes] = useState<number>(readSavedMinutes);

  const isIdle = status === 'idle' || status === 'finished' || status === 'error';
  const isRunning = !isIdle && isQuestRun;
  const justFinished = isIdle && status === 'finished' && isQuestRun;

  useEffect(() => {
    tauriApi
      .getGamesList()
      .then(setGames)
      .catch((err) => console.error('Failed to load games list:', err));
  }, [isFetching]);

  useEffect(() => {
    writeSavedGameIds(selectedIds);
  }, [selectedIds]);

  useEffect(() => {
    writeSavedMinutes(minutes);
  }, [minutes]);

  // Discord only credits quests for real game processes, so the list is process-only
  // here regardless of the beta/RPC toggle on the dashboard.
  const availableGames = useMemo(
    () => games.filter((g) => g.method === 'process' && !(excludeAdult && g.adult)),
    [games, excludeAdult]
  );

  const selectedGames = useMemo(
    () =>
      selectedIds
        .map((id) => availableGames.find((g) => g.id === id))
        .filter((g): g is SpoofableGameItem => Boolean(g)),
    [selectedIds, availableGames]
  );

  const usedExes = useMemo(
    () => new Set(selectedGames.map(exeBasename).filter(Boolean)),
    [selectedGames]
  );

  const searchResults = useMemo(() => {
    const term = searchTerm.trim().toLowerCase();
    if (!term) return [];
    return availableGames
      .filter((g) => !selectedIds.includes(g.id))
      .filter(
        (g) =>
          g.name.toLowerCase().includes(term) ||
          (g.rel_path && g.rel_path.toLowerCase().includes(term)) ||
          g.id.includes(term)
      )
      .slice(0, MAX_SEARCH_RESULTS);
  }, [searchTerm, availableGames, selectedIds]);

  const isFull = selectedGames.length >= MAX_QUEST_GAMES;
  const activeIds = new Set(tickData.active_batch.map((g) => g.id));

  const totalSecs = minutes * 60;
  const elapsed = Math.min(tickData.elapsed_secs, totalSecs);
  const remaining = Math.max(totalSecs - tickData.elapsed_secs, 0);
  const progress = totalSecs > 0 ? Math.min((elapsed / totalSecs) * 100, 100) : 0;

  const commitMinutes = (value: string) => {
    const parsed = Math.round(Number(value));
    if (!Number.isFinite(parsed)) {
      setMinutes(DEFAULT_MINUTES);
      return;
    }
    setMinutes(Math.min(Math.max(parsed, MIN_MINUTES), MAX_MINUTES));
  };

  return (
    <div className="flex flex-col h-full gap-3 overflow-hidden pr-1">
      {/* Status and controls */}
      <div className="bg-[#161722] border border-[#232535] rounded-xl p-4 shadow-sm flex flex-col gap-3 flex-shrink-0">
        <div>
          <h2 className="text-sm font-semibold text-white flex items-center gap-2">
            <Target className="w-4 h-4 text-[#5865F2]" />
            {t('quest.title')}
          </h2>
          <p className="text-xs text-neutral-400 mt-0.5">{t('quest.subtitle')}</p>
        </div>

        <div className="flex flex-col sm:flex-row sm:items-center gap-3">
          <div className="flex items-center gap-1 flex-1">
            {MINUTE_PRESETS.map((preset) => (
              <button
                key={preset}
                type="button"
                disabled={!isIdle}
                onClick={() => setMinutes(preset)}
                className={`py-1.5 px-2.5 text-[11px] rounded border transition-all disabled:opacity-50 ${
                  minutes === preset
                    ? 'bg-[#5865F2] text-white border-[#5865F2] font-semibold'
                    : 'bg-[#1b1c28] text-neutral-400 border-white/10 hover:text-white'
                }`}
              >
                {t('quest.minutes', { count: preset })}
              </button>
            ))}

            <div className="flex items-center gap-1.5 ml-1">
              <input
                type="number"
                min={MIN_MINUTES}
                max={MAX_MINUTES}
                value={minutes}
                disabled={!isIdle}
                onChange={(e) => commitMinutes(e.target.value)}
                className="w-16 bg-[#1b1c28] border border-white/10 rounded px-2 py-1.5 text-[11px] text-white focus:outline-none focus:border-[#5865F2] disabled:opacity-50"
              />
              <span className="text-[11px] text-neutral-400">{t('quest.minLabel')}</span>
            </div>
          </div>

          {isRunning ? (
            <div className="flex items-center gap-3">
              <div className="text-xs text-neutral-400 whitespace-nowrap">
                {t('quest.left')}{' '}
                <span className="font-mono font-bold text-emerald-400">{formatClock(remaining)}</span>
              </div>
              <button
                type="button"
                onClick={onStop}
                className="py-2 px-6 rounded-lg bg-[#3d181e] hover:bg-[#521e27] text-white font-semibold text-xs uppercase tracking-wider transition-all"
              >
                {t('quest.stop')}
              </button>
            </div>
          ) : (
            <button
              type="button"
              disabled={!isIdle || selectedGames.length === 0}
              onClick={() => onStart(selectedGames.map((g) => g.id), totalSecs)}
              className="py-2 px-6 rounded-lg bg-[#5865F2] hover:bg-[#4752c4] text-white font-semibold text-xs uppercase tracking-wider transition-all disabled:opacity-40 disabled:cursor-not-allowed"
            >
              {t('quest.start')}
            </button>
          )}
        </div>

        {isRunning && (
          <div>
            <div className="h-1.5 rounded-full bg-[#1b1c28] overflow-hidden">
              <div
                className="h-full bg-[#5865F2] transition-all duration-500"
                style={{ width: `${progress}%` }}
              />
            </div>
            <div className="mt-1.5 text-[11px] text-neutral-400">{t('quest.running')}</div>
          </div>
        )}

        {!isIdle && !isQuestRun && (
          <div className="text-[11px] text-amber-400">{t('quest.busy')}</div>
        )}

        {justFinished && (
          <div className="flex items-start gap-2 rounded-lg bg-emerald-500/10 border border-emerald-500/30 p-2.5">
            <CheckCircle2 className="w-4 h-4 text-emerald-400 flex-shrink-0 mt-0.5" />
            <div>
              <div className="text-xs font-semibold text-emerald-400">{t('quest.doneTitle')}</div>
              <div className="text-[11px] text-neutral-400">{t('quest.doneBody')}</div>
            </div>
          </div>
        )}
      </div>

      <div className="grid grid-cols-1 md:grid-cols-2 gap-3 flex-1 min-h-0 overflow-hidden">
        {/* Quest game set */}
        <div className="bg-[#161722] border border-[#232535] rounded-xl p-4 flex flex-col shadow-sm min-h-0 overflow-hidden">
          <div className="flex items-center justify-between pb-2 border-b border-white/5 flex-shrink-0">
            <h3 className="text-sm font-semibold text-white">{t('quest.selected')}</h3>
            <span className="text-[11px] font-mono text-neutral-400">
              {selectedGames.length}/{MAX_QUEST_GAMES}
            </span>
          </div>

          <div className="flex-1 overflow-y-auto pr-1 mt-2 space-y-1.5 min-h-0">
            {selectedGames.length === 0 ? (
              <div className="h-full flex items-center justify-center text-center text-xs text-neutral-500 p-4">
                {t('quest.emptySelection')}
              </div>
            ) : (
              selectedGames.map((game) => (
                <div
                  key={game.id}
                  className="flex items-center justify-between gap-2 p-2 rounded-lg bg-[#1b1c28] border border-white/5 text-xs"
                >
                  <div className="flex items-center gap-2.5 min-w-0">
                    <GameIcon game={game} />
                    <span className="font-medium text-white truncate">{game.name}</span>
                  </div>
                  {isRunning ? (
                    <span
                      className={`w-2.5 h-2.5 rounded-full flex-shrink-0 ${
                        activeIds.has(game.id) ? 'bg-emerald-500' : 'bg-neutral-600'
                      }`}
                    />
                  ) : (
                    <button
                      type="button"
                      disabled={!isIdle}
                      onClick={() => setSelectedIds((prev) => prev.filter((id) => id !== game.id))}
                      className="p-1 rounded text-neutral-400 hover:text-white hover:bg-white/5 flex-shrink-0 disabled:opacity-40"
                      title={t('quest.remove')}
                    >
                      <X className="w-3.5 h-3.5" />
                    </button>
                  )}
                </div>
              ))
            )}
          </div>

          <div className="mt-2 pt-2 border-t border-white/5 text-[11px] text-neutral-500 flex-shrink-0">
            {t('quest.hint')}
          </div>
        </div>

        {/* Game search */}
        <div className="bg-[#161722] border border-[#232535] rounded-xl p-4 flex flex-col shadow-sm min-h-0 overflow-hidden">
          <div className="relative flex-shrink-0">
            <Search className="w-3.5 h-3.5 text-neutral-400 absolute left-2.5 top-1/2 -translate-y-1/2" />
            <input
              type="text"
              placeholder={t('database.search')}
              value={searchTerm}
              onChange={(e) => setSearchTerm(e.target.value)}
              className="w-full bg-[#1b1c28] border border-white/10 rounded-lg pl-8 pr-3 py-1.5 text-xs text-white placeholder-neutral-500 focus:outline-none focus:border-[#5865F2]"
            />
          </div>

          <div className="flex-1 overflow-y-auto pr-1 mt-2 space-y-1.5 min-h-0">
            {searchResults.length === 0 ? (
              <div className="h-full flex items-center justify-center text-center text-xs text-neutral-500 p-4">
                {searchTerm.trim()
                  ? t('database.empty')
                  : t('quest.searchHint', { count: formatNumber(availableGames.length) })}
              </div>
            ) : (
              searchResults.map((game) => {
                const exe = exeBasename(game);
                const hasExeConflict = exe !== null && usedExes.has(exe);
                return (
                  <div
                    key={game.id}
                    className="flex items-center justify-between gap-2 p-2 rounded-lg bg-[#1b1c28] border border-white/5 text-xs"
                  >
                    <div className="flex items-center gap-2.5 min-w-0">
                      <GameIcon game={game} />
                      <div className="min-w-0">
                        <div className="font-medium text-white truncate">{game.name}</div>
                        {hasExeConflict && (
                          <div className="text-[10px] text-amber-400 truncate">
                            {t('farm.exeConflict', { exe })}
                          </div>
                        )}
                      </div>
                    </div>
                    <button
                      type="button"
                      disabled={!isIdle || isFull || hasExeConflict}
                      onClick={() => setSelectedIds((prev) => [...prev, game.id])}
                      className="p-1 rounded text-neutral-400 hover:text-white hover:bg-white/5 flex-shrink-0 disabled:opacity-30 disabled:cursor-not-allowed"
                      title={t('quest.add')}
                    >
                      <Plus className="w-3.5 h-3.5" />
                    </button>
                  </div>
                );
              })
            )}
          </div>
        </div>
      </div>
    </div>
  );
};
