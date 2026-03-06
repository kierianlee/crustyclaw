import { createFileRoute } from "@tanstack/react-router";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Save, Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Card,
  CardHeader,
  CardTitle,
  CardPanel,
  CardAction,
} from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { Tabs, TabsList, TabsTab, TabsPanel } from "@/components/ui/tabs";
import { Badge } from "@/components/ui/badge";
import type { SoulFile, SoulResponse } from "@/types/bindings";

function SoulPage() {
  const queryClient = useQueryClient();
  const { data, isLoading } = useQuery<SoulResponse>({
    queryKey: ["soul"],
    queryFn: () => fetch("/api/soul").then((r) => r.json()),
  });

  const [draft, setDraft] = useState<SoulFile[] | null>(null);
  const [newName, setNewName] = useState("");

  const files = draft ?? data?.files ?? [];
  const dirty = draft !== null;

  const saveMutation = useMutation({
    mutationFn: async () => {
      const res = await fetch("/api/soul", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ files }),
      });
      if (!res.ok) throw new Error(await res.text());
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["soul"] });
      setDraft(null);
    },
  });

  const updateContent = (index: number, content: string) => {
    setDraft(files.map((f, i) => (i === index ? { ...f, content } : f)));
  };

  const removeFile = (index: number) => {
    setDraft(files.filter((_, i) => i !== index));
  };

  const addFile = () => {
    const name = newName.trim();
    if (!name) return;
    const fname = name.endsWith(".md") ? name : `${name}.md`;
    if (files.some((f) => f.name === fname)) return;
    setDraft([...files, { name: fname, content: "" }]);
    setNewName("");
  };

  if (isLoading)
    return (
      <div className="flex justify-center p-8">
        <Spinner />
      </div>
    );

  return (
    <div className="space-y-4 max-w-3xl">
      <div className="flex items-center justify-between">
        <h2 className="text-lg font-semibold">Soul Prompts</h2>
        <div className="flex items-center gap-2">
          {dirty && (
            <Badge variant="warning" size="sm">
              Unsaved
            </Badge>
          )}
          <Button
            size="sm"
            onClick={() => saveMutation.mutate()}
            disabled={!dirty || saveMutation.isPending}
          >
            <Save /> {saveMutation.isPending ? "Saving..." : "Save All"}
          </Button>
        </div>
      </div>

      {saveMutation.error && (
        <p className="text-destructive-foreground text-sm">
          {(saveMutation.error as Error).message}
        </p>
      )}

      {files.length === 0 ? (
        <p className="text-muted-foreground text-sm">No prompt files yet.</p>
      ) : (
        <Tabs defaultValue={files[0]?.name}>
          <TabsList>
            {files.map((f) => (
              <TabsTab key={f.name} value={f.name}>
                {f.name}
              </TabsTab>
            ))}
          </TabsList>
          {files.map((f, i) => (
            <TabsPanel key={f.name} value={f.name}>
              <Card className="mt-2">
                <CardHeader>
                  <CardTitle>{f.name}</CardTitle>
                  <CardAction>
                    <Button
                      variant="ghost"
                      size="icon-xs"
                      onClick={() => removeFile(i)}
                    >
                      <Trash2 />
                    </Button>
                  </CardAction>
                </CardHeader>
                <CardPanel>
                  <Textarea
                    value={f.content}
                    onChange={(e) => updateContent(i, e.target.value)}
                    rows={16}
                    className="font-mono text-sm"
                  />
                </CardPanel>
              </Card>
            </TabsPanel>
          ))}
        </Tabs>
      )}

      <Card>
        <CardPanel>
          <div className="flex gap-2">
            <Input
              placeholder="new-prompt.md"
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && addFile()}
            />
            <Button
              variant="outline"
              onClick={addFile}
              disabled={!newName.trim()}
            >
              <Plus /> Add
            </Button>
          </div>
        </CardPanel>
      </Card>
    </div>
  );
}

export const Route = createFileRoute("/soul")({ component: SoulPage });
