import { useLocation } from "@tanstack/react-router";
import { ModeToggle } from "@/components/ModeToggle/ModeToggle";
import { SidebarTrigger } from "@/components/ui/sidebar";

const getPageTitle = (pathname: string): string => {
  switch (pathname) {
    case "/":
      return "Dashboard";
    case "/logs":
      return "Logs";
    case "/jobs":
      return "Jobs";
    case "/search":
      return "Search";
    case "/settings":
    case "/settings/general":
      return "Settings";
    default:
      return "crust-seed";
  }
};

const Header = () => {
  const location = useLocation();
  const pageTitle = getPageTitle(location.pathname);

  return (
    <header className="bg-background/85 supports-[backdrop-filter]:bg-background/65 border-border/70 sticky top-0 z-20 flex shrink-0 items-center gap-3 border-b px-4 py-3 backdrop-blur-md sm:px-6">
      <SidebarTrigger className="text-muted-foreground hover:text-foreground -ml-1 size-8 rounded-lg" />
      <h1 className="truncate text-lg leading-tight font-semibold">
        {pageTitle}
      </h1>
      <div className="ml-auto">
        <ModeToggle className="text-muted-foreground hover:text-foreground size-8 rounded-lg" />
      </div>
    </header>
  );
};

export default Header;
