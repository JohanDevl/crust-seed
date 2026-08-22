import { LoginForm } from "@/components/login-form";
import { useTRPC } from "@/lib/trpc";
import { useSuspenseQuery } from "@tanstack/react-query";
import type { ReactNode } from "react";

type AuthWrapperProps = {
  children: ReactNode;
};

export function Login({ children }: AuthWrapperProps) {
  const trpc = useTRPC();
  const { data: authStatus } = useSuspenseQuery(
    trpc.auth.authStatus.queryOptions(),
  );

  // If not logged in, show login form
  if (!authStatus?.isLoggedIn) {
    return (
      <div className="bg-sidebar relative flex min-h-screen items-center justify-center overflow-hidden p-4">
        {/* Same warm wash the app shell sits on, so the login screen belongs
            to the product rather than looking like a bare form. */}
        <div
          aria-hidden
          className="pointer-events-none absolute inset-0"
          style={{
            backgroundImage: `
              radial-gradient(760px 520px at 15% 0%, color-mix(in oklab, var(--brand-shell) 20%, transparent), transparent 62%),
              radial-gradient(680px 460px at 88% 100%, color-mix(in oklab, var(--brand-core) 16%, transparent), transparent 58%)
            `,
          }}
        />
        <div className="relative w-full max-w-md">
          <LoginForm />
        </div>
      </div>
    );
  }

  return children;
}
