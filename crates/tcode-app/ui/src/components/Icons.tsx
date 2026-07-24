/**
 * The icon set. One geometry for all of them — a 24-unit grid, 2.2 stroke,
 * round caps and joins — so they read as siblings of the mark rather than as
 * a pack someone imported.
 */
type IconProps = { size?: number; className?: string };

function Glyph({ size = 16, className, children }: IconProps & { children: React.ReactNode }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2.2"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      aria-hidden="true"
      focusable="false"
    >
      {children}
    </svg>
  );
}

export const FolderIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M3 7.5a2 2 0 0 1 2-2h3.6l2 2.5H19a2 2 0 0 1 2 2v7.5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z" />
  </Glyph>
);

export const ChevronRight = (props: IconProps) => (
  <Glyph {...props}>
    <path d="m9.5 5.5 6.5 6.5-6.5 6.5" />
  </Glyph>
);

export const ChevronDown = (props: IconProps) => (
  <Glyph {...props}>
    <path d="m5.5 9.5 6.5 6.5 6.5-6.5" />
  </Glyph>
);

export const ArrowUp = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M12 19V5.5" />
    <path d="m5.5 12 6.5-6.5 6.5 6.5" />
  </Glyph>
);

export const StopIcon = (props: IconProps) => (
  <Glyph {...props}>
    <rect x="6.5" y="6.5" width="11" height="11" rx="2" fill="currentColor" />
  </Glyph>
);

export const CloseIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M6.5 6.5 17.5 17.5" />
    <path d="M17.5 6.5 6.5 17.5" />
  </Glyph>
);

export const FileIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M13.5 3.5H7a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V9Z" />
    <path d="M13.5 3.5V9H19" />
  </Glyph>
);

export const PanelIcon = (props: IconProps) => (
  <Glyph {...props}>
    <rect x="3.5" y="4.5" width="17" height="15" rx="2" />
    <path d="M14.5 4.5v15" />
  </Glyph>
);

export const PlusIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M12 5.5v13" />
    <path d="M5.5 12h13" />
  </Glyph>
);

export const BackIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M19 12H5.5" />
    <path d="m12 5.5-6.5 6.5 6.5 6.5" />
  </Glyph>
);
