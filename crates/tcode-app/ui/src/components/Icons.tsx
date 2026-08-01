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

/** The return key's own glyph. Used where the affordance *is* the key rather
 *  than a button that happens to be bound to it. */
export const ReturnIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M19 6v5.5a2 2 0 0 1-2 2H6" />
    <path d="m9.5 10 -3.5 3.5 3.5 3.5" />
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

/** The rail, not a side panel: the divider sits on the *left* third, mirroring
 *  `PanelIcon`, so the window's own sidebar and a pane's files panel are never
 *  the same glyph pointing at two different things. */
export const SidebarIcon = (props: IconProps) => (
  <Glyph {...props}>
    <rect x="3.5" y="4.5" width="17" height="15" rx="2" />
    <path d="M9.5 4.5v15" />
  </Glyph>
);

export const ExpandIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M9 5H5v4" />
    <path d="m5 5 5 5" />
    <path d="M15 19h4v-4" />
    <path d="m19 19-5-5" />
  </Glyph>
);

export const CollapseIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M5 9h4V5" />
    <path d="m9 9-4-4" />
    <path d="M19 15h-4v4" />
    <path d="m15 15 4 4" />
  </Glyph>
);

export const PlusIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M12 5.5v13" />
    <path d="M5.5 12h13" />
  </Glyph>
);

export const SearchIcon = (props: IconProps) => (
  <Glyph {...props}>
    <circle cx="10.8" cy="10.8" r="6.3" />
    <path d="m15.4 15.4 4.1 4.1" />
  </Glyph>
);

/** A turn all the way round, back to where it started — re-reading what is
 *  already on screen rather than fetching something new. The head sits exactly
 *  on the arc's end so the stroke reads as one motion. */
export const RefreshIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3" />
    <path d="M14.3 6.7h3v-3" />
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

/* An arrow returning to where it came from. Deliberately not a trash can: what
   this offers is going back to a point, and only incidentally losing what came
   after. */
export const RewindIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M4 5v6h6" />
    <path d="M4.5 11a8 8 0 1 1 2 6" />
  </Glyph>
);

/* A nib, not a full pencil: at 14px the eraser and ferrule of a whole pencil
   collapse into noise, and what this control means is "write on it". */
export const PencilIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M4 20h4L19.5 8.5a2.5 2.5 0 0 0-3.5-3.5L4.5 16.5Z" />
    <path d="m15 6 3 3" />
  </Glyph>
);

/* Sliders rather than a cog: what this opens is a short list of switches, and a
   cog in a title bar promises the application's whole preferences. */
/** The browser. A globe rather than a window outline, which in a window full
 *  of panes would name the wrong thing entirely. */
export const GlobeIcon = (props: IconProps) => (
  <Glyph {...props}>
    <circle cx="12" cy="12" r="9" />
    <path d="M3 12h18" />
    <path d="M12 3a14 14 0 0 1 0 18a14 14 0 0 1 0-18Z" />
  </Glyph>
);

export const SettingsIcon = (props: IconProps) => (
  <Glyph {...props}>
    <path d="M4 8h10M18 8h2M4 16h4M12 16h8" />
    <circle cx="16" cy="8" r="2.3" />
    <circle cx="10" cy="16" r="2.3" />
  </Glyph>
);
