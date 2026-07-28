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

export const ForwardIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M5 12h13.5" />
    <path d="m12 5.5 6.5 6.5-6.5 6.5" />
  </Glyph>
);

export const CopyIcon = (props: IconProps) => (
  <Glyph {...props}>
    <rect x="9" y="9" width="11" height="11" rx="2" />
    <path d="M15 5.5H6a1.5 1.5 0 0 0-1.5 1.5v9" />
  </Glyph>
);

/* Window controls. Squarer than the rest on purpose: they are the caption bar's
   glyphs, and the platform's own vocabulary is what makes them read as such. */

export const MinimizeIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M5.5 12h13" />
  </Glyph>
);

export const MaximizeIcon = (props: IconProps) => (
  <Glyph {...props}>
    <rect x="5.5" y="5.5" width="13" height="13" rx="1.5" />
  </Glyph>
);

export const RestoreIcon = (props: IconProps) => (
  <Glyph {...props}>
    <rect x="4.5" y="7.5" width="11" height="11" rx="1.5" />
    <path d="M8.5 5.5h9a2 2 0 0 1 2 2v9" />
  </Glyph>
);

export const ImageIcon = (props: IconProps) => (
  <Glyph {...props}>
    <rect x="3.5" y="4.5" width="17" height="15" rx="2" />
    <path d="m4 16 4.5-4.5 4 4 3-2.5 4 3.5" />
  </Glyph>
);

export const CheckIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="m5 12.5 4.5 4.5L19 7" />
  </Glyph>
);
