import { createFileRoute } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import { Badge } from "@/components/ui/badge";
import { Card, CardPanel } from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import type { ChatEntry } from "@/types/bindings";

function ChatPage() {
  const { data: entries, isLoading } = useQuery<ChatEntry[]>({
    queryKey: ["chat"],
    queryFn: () => fetch("/api/chat").then((r) => r.json()),
    refetchInterval: 2000,
  });

  if (isLoading)
    return (
      <div className="flex justify-center p-8">
        <Spinner />
      </div>
    );

  if (!entries || entries.length === 0) {
    return <p className="text-muted-foreground text-sm">No messages yet.</p>;
  }

  return (
    <div className="flex flex-col gap-2 max-w-2xl">
      {entries.map((entry, i) => (
        <Card
          key={i}
          className={entry.direction === "outgoing" ? "ml-8" : "mr-8"}
        >
          <CardPanel className="p-3">
            <div className="flex items-start justify-between gap-2 mb-1">
              <Badge
                variant={entry.direction === "incoming" ? "info" : "success"}
                size="sm"
              >
                {entry.direction === "incoming" ? "User" : "Bot"}
              </Badge>
              <span className="text-muted-foreground text-xs shrink-0">
                {new Date(entry.timestamp).toLocaleTimeString()}
              </span>
            </div>
            <p className="text-sm whitespace-pre-wrap wrap-break-word">
              {entry.text}
            </p>
          </CardPanel>
        </Card>
      ))}
    </div>
  );
}

export const Route = createFileRoute("/chat")({ component: ChatPage });
