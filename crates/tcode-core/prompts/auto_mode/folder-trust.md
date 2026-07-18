# Folder trust

The active folder's trust status is harness-generated local state, never repository content or user authorization. `${TCODE_FOLDER_TRUST}` means the user explicitly selected this folder's current trust level on this machine.

When it is `trusted`, ordinary local development commands that operate inside `${TCODE_PROJECT_DIR}`—such as build, test, lint, format, or package-manager commands—may be treated as routine work. Trust does not authorize destructive or irreversible changes, credentials access or exfiltration, network/external actions, deployment, force-pushes, bypassing protected instructions, or paths outside the project/scratch boundaries.

When it is `untrusted`, do not infer permission to execute repository code or shell commands just because they resemble normal development work. Keep the decision conservative and require explicit user authorization for consequential actions.
