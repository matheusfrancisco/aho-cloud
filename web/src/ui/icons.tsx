// Inline stroke icons (16×16 grid, currentColor). Decorative: hidden from assistive tech.
import type { ReactNode } from "react";

function Svg({ children, size = 16 }: { children: ReactNode; size?: number }) {
  return (
    <svg className="icon" width={size} height={size} viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth={1.5} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      {children}
    </svg>
  );
}

export function LogoMark() {
  return (
    <svg className="logo-mark" width={30} height={30} viewBox="0 0 30 30" aria-hidden="true" focusable="false">
      <defs>
        <linearGradient id="logo-gradient" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#8b5cf6" />
          <stop offset="1" stopColor="#5b21b6" />
        </linearGradient>
      </defs>
      <rect width="30" height="30" rx="8" fill="url(#logo-gradient)" />
      <path d="M9.5 19.5h11a3.5 3.5 0 0 0 .4-7 5 5 0 0 0-9.6-1.3A3.9 3.9 0 0 0 9.5 19.5Z" fill="#fff" />
      <circle cx="14" cy="15.5" r="1.1" fill="#6d28d9" />
      <circle cx="17.6" cy="15.5" r="1.1" fill="#6d28d9" />
    </svg>
  );
}

export const ProjectsIcon = () => (
  <Svg>
    <rect x="2.5" y="2.5" width="4.5" height="4.5" rx="1" />
    <rect x="9" y="2.5" width="4.5" height="4.5" rx="1" />
    <rect x="2.5" y="9" width="4.5" height="4.5" rx="1" />
    <rect x="9" y="9" width="4.5" height="4.5" rx="1" />
  </Svg>
);

export const TeamIcon = () => (
  <Svg>
    <circle cx="6" cy="5.5" r="2.25" />
    <path d="M2 13c.4-2.2 2-3.5 4-3.5s3.6 1.3 4 3.5" />
    <path d="M10.5 3.5a2.2 2.2 0 0 1 0 4.2M11.5 9.7c1.3.4 2.2 1.5 2.5 3.3" />
  </Svg>
);

export const PolicyIcon = () => (
  <Svg>
    <path d="M8 1.8 13 3.6v4c0 3-2.1 5.4-5 6.6-2.9-1.2-5-3.6-5-6.6v-4L8 1.8Z" />
    <path d="m5.8 8 1.5 1.5 2.9-3" />
  </Svg>
);

export const PlusIcon = () => (
  <Svg>
    <path d="M8 3v10M3 8h10" />
  </Svg>
);

export const ChevronDown = () => (
  <Svg size={14}>
    <path d="m4 6 4 4 4-4" />
  </Svg>
);

export const ChevronRight = () => (
  <Svg size={14}>
    <path d="m6 4 4 4-4 4" />
  </Svg>
);

export const SearchIcon = () => (
  <Svg>
    <circle cx="7" cy="7" r="4.5" />
    <path d="m10.5 10.5 3 3" />
  </Svg>
);

export const CopyIcon = () => (
  <Svg>
    <rect x="5.5" y="5.5" width="8" height="8" rx="1.5" />
    <path d="M10.5 5.5v-2a1 1 0 0 0-1-1h-6a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2" />
  </Svg>
);

export const CheckIcon = () => (
  <Svg>
    <path d="m3.5 8.5 3 3 6-7" />
  </Svg>
);

export const FolderIcon = () => (
  <Svg>
    <path d="M2 4.5a1 1 0 0 1 1-1h3.2l1.5 1.5H13a1 1 0 0 1 1 1V12a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V4.5Z" />
  </Svg>
);

export const FileIcon = () => (
  <Svg>
    <path d="M4 1.8h5l3 3v9.4H4V1.8Z" />
    <path d="M9 1.8v3h3" />
  </Svg>
);

export const RefreshIcon = () => (
  <Svg>
    <path d="M13 3.5v3h-3" />
    <path d="M12.6 6.5A5 5 0 1 0 13 10" />
  </Svg>
);

export const UserIcon = () => (
  <Svg>
    <circle cx="8" cy="5.5" r="2.75" />
    <path d="M2.5 14c.5-2.8 2.7-4.5 5.5-4.5s5 1.7 5.5 4.5" />
  </Svg>
);

export const OrgIcon = () => (
  <Svg>
    <path d="M8 2.2 9.3 6.7 13.8 8 9.3 9.3 8 13.8 6.7 9.3 2.2 8l4.5-1.3L8 2.2Z" />
  </Svg>
);

export const RocketIcon = () => (
  <Svg>
    <path d="M9.5 2.5c2.4-.7 4 .9 4 .9s1.6 1.6.9 4L10 11.8 4.2 6 9.5 2.5Z" />
    <path d="M6 7.8 3.8 7.2 2.5 9.8l2.2.2M8.2 10l.6 2.2-2.6 1.3-.2-2.2" />
    <circle cx="10.5" cy="5.5" r="1" />
  </Svg>
);

export const GlobeIcon = () => (
  <Svg>
    <circle cx="8" cy="8" r="6" />
    <path d="M2 8h12M8 2c1.7 1.7 2.5 3.7 2.5 6S9.7 12.3 8 14c-1.7-1.7-2.5-3.7-2.5-6S6.3 3.7 8 2Z" />
  </Svg>
);

export const ExternalIcon = () => (
  <Svg size={12}>
    <path d="M9.5 2.5h4v4M13.5 2.5 8 8M11.5 9.5v3.5a.5.5 0 0 1-.5.5H3a.5.5 0 0 1-.5-.5V5a.5.5 0 0 1 .5-.5h3.5" />
  </Svg>
);

export const CloseIcon = () => (
  <Svg>
    <path d="m4 4 8 8M12 4l-8 8" />
  </Svg>
);

/** The small bordered square the sidebar and menus put icons in. */
export function IconSquare({ children }: { children: ReactNode }) {
  return <span className="icon-square">{children}</span>;
}

/** Empty-state art: a little lavender house with a cloud, drawn inline. */
export function EmptyIllustration() {
  return (
    <svg className="empty-art" width="160" height="120" viewBox="0 0 160 120" aria-hidden="true" focusable="false">
      <ellipse cx="80" cy="108" rx="56" ry="6" fill="#ede9f7" />
      <path d="M40 54 80 26l40 28v52H40V54Z" fill="#f4f0ff" stroke="#6d28d9" strokeWidth="2" strokeLinejoin="round" />
      <path d="M34 58 80 24l46 34" fill="none" stroke="#6d28d9" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" />
      <path d="M52 62h14v14H52zM94 62h14v14H94z" fill="#fff" stroke="#a78bfa" strokeWidth="2" />
      <path d="M72 106V84a8 8 0 0 1 16 0v22" fill="#ddd6fe" stroke="#6d28d9" strokeWidth="2" />
      <path d="M104 22h24a8 8 0 0 0 .6-16 11 11 0 0 0-21.1-2.6A8.5 8.5 0 0 0 104 22Z" fill="#fff" stroke="#c4b5fd" strokeWidth="2" transform="translate(4 8)" />
      <circle cx="30" cy="96" r="7" fill="#c4b5fd" />
      <circle cx="132" cy="98" r="5" fill="#ddd6fe" />
    </svg>
  );
}
