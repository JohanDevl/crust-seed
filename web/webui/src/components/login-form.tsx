import { useState } from "react";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { LogoMark } from "@/components/brand/LogoMark";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useTRPC } from "@/lib/trpc";
import {
  useMutation,
  useSuspenseQuery,
  useQueryClient,
} from "@tanstack/react-query";

export function LoginForm({
  className,
  ...props
}: React.ComponentProps<"div">) {
  const queryClient = useQueryClient();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const trpc = useTRPC();

  const { data: authStatus } = useSuspenseQuery(
    trpc.auth.authStatus.queryOptions(),
  );

  const { mutate: login, isPending } = useMutation(
    trpc.auth.logIn.mutationOptions({
      onSuccess: () => {
        void queryClient.invalidateQueries({
          queryKey: trpc.auth.authStatus.queryKey(),
        });
      },
      onError: (error) => {
        const message =
          error?.data?.code === "UNAUTHORIZED"
            ? "Invalid username or password"
            : (error?.message ?? "Login failed");
        setError(message);
      },
    }),
  );

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setError("");
    login({ username, password });
  };

  const isSignUp = !authStatus.userExists;
  const signupAllowed = authStatus.signupAllowed;
  const isDocker = authStatus.isDocker;
  const resetCommand = isDocker
    ? "docker exec -it <container> crust-seed reset-user"
    : "crust-seed reset-user";
  const disabled = isPending || (isSignUp && !signupAllowed);

  return (
    <div className={cn("flex flex-col gap-6", className)} {...props}>
      <div className="flex flex-col items-center gap-3 text-center">
        <span className="bg-card ring-border/70 flex size-14 items-center justify-center rounded-2xl shadow-md ring-1">
          <LogoMark className="size-8" />
        </span>
        <span className="text-2xl font-semibold tracking-tight">
          crust<span className="text-primary">-seed</span>
        </span>
      </div>

      <Card className="shadow-xl">
        <CardHeader>
          <CardTitle>{isSignUp ? "Initial setup" : "Welcome back"}</CardTitle>
          <CardDescription>
            {isSignUp
              ? "Create the account you will use to manage this instance."
              : "Sign in to manage indexers, clients and cross-seeding."}
          </CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={handleSubmit}>
            <div className="flex flex-col gap-5">
              {isSignUp && signupAllowed && (
                <p className="bg-info/10 text-info border-info/20 rounded-lg border px-3 py-2 text-sm">
                  For security reasons, initial setup is only available for 5
                  minutes after crust-seed starts.
                </p>
              )}
              {isSignUp && !signupAllowed && (
                <p className="bg-destructive/10 text-destructive border-destructive/20 rounded-lg border px-3 py-2 text-sm font-medium">
                  Setup window closed for security reasons. Restart crust-seed
                  to create the first user.
                </p>
              )}
              {error && (
                <div className="bg-destructive/10 text-destructive border-destructive/20 rounded-lg border px-3 py-2 text-sm font-medium">
                  {error}
                </div>
              )}
              <div className="grid gap-2">
                <Label htmlFor="username">Username</Label>
                <Input
                  id="username"
                  placeholder="admin"
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                  disabled={disabled}
                  required
                />
              </div>
              <div className="grid gap-2">
                <Label htmlFor="password">Password</Label>
                <Input
                  id="password"
                  type="password"
                  placeholder="••••••••••"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  disabled={disabled}
                  required
                />
              </div>
              <Button
                type="submit"
                size="lg"
                disabled={disabled}
                className="w-full"
              >
                {isPending
                  ? "Processing..."
                  : isSignUp
                    ? "Create account"
                    : "Sign in"}
              </Button>
              {!isSignUp && (
                <p className="text-muted-foreground text-center text-xs">
                  Forgot your password? Run{" "}
                  <code className="bg-muted text-foreground rounded px-1.5 py-0.5 font-mono text-[0.7rem]">
                    {resetCommand}
                  </code>
                </p>
              )}
            </div>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
