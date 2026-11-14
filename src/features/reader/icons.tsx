// Toolbar icons (inline SVG, currentColor). Decorative: buttons carry the aria-label.
const base = { width: 16, height: 16, viewBox: '0 0 16 16', fill: 'none', stroke: 'currentColor', strokeWidth: 1.5, strokeLinecap: 'round', strokeLinejoin: 'round', 'aria-hidden': true } as const;

export const IconPause = () => (
  <svg {...base}><rect x="3.5" y="3" width="3" height="10" rx="0.8" fill="currentColor" stroke="none" /><rect x="9.5" y="3" width="3" height="10" rx="0.8" fill="currentColor" stroke="none" /></svg>
);

export const IconPlay = () => (
  <svg {...base}><path d="M4.5 3.2v9.6a.5.5 0 0 0 .77.42l7.3-4.8a.5.5 0 0 0 0-.84l-7.3-4.8a.5.5 0 0 0-.77.42Z" fill="currentColor" stroke="none" /></svg>
);

export const IconGear = () => (
  <svg {...base}>
    <circle cx="8" cy="8" r="2.2" />
    <path d="M8 1.6v1.7M8 12.7v1.7M1.6 8h1.7M12.7 8h1.7M3.5 3.5l1.2 1.2M11.3 11.3l1.2 1.2M12.5 3.5l-1.2 1.2M4.7 11.3l-1.2 1.2" />
  </svg>
);

export const IconSidebar = () => (
  <svg {...base}><rect x="1.8" y="2.8" width="12.4" height="10.4" rx="1.8" /><path d="M6 2.8v10.4" /></svg>
);
