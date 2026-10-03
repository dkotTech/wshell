const PATHS = {
  start: 'M8 5v14l11-7z',
  stop: 'M7 7h10v10H7z',
  restart: 'M4 12a8 8 0 1 0 2.3-5.6M4 4v4h4',
  open: 'M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5',
  logs: 'M5 5h14M5 10h14M5 15h9M5 20h6',
  trash: 'M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13',
  download: 'M12 4v12m0 0-5-5m5 5 5-5M5 20h14',
} as const;

export type IconName = keyof typeof PATHS;

export function Icon({ name, size }: { name: IconName; size?: number }) {
  const filled = name === 'start' || name === 'stop';
  return (
    <svg viewBox="0 0 24 24" width={size} height={size} aria-hidden="true">
      <path
        d={PATHS[name]}
        fill={filled ? 'currentColor' : 'none'}
        stroke="currentColor"
        stroke-width={filled ? 1 : 2}
        stroke-linecap="round"
        stroke-linejoin="round"
      />
    </svg>
  );
}

export function Logo() {
  return (
    <svg viewBox="0 0 24 24" width="22" height="22" aria-hidden="true">
      <path d="M12 2 3 7v10l9 5 9-5V7z" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" />
      <path d="m3 7 9 5 9-5M12 12v10" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" />
    </svg>
  );
}
