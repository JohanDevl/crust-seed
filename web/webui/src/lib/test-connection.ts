import { trpcClient } from "./trpc";

type DownloadClientKind =
  | "qbittorrent"
  | "rtorrent"
  | "transmission"
  | "deluge";

function isDownloadClientKind(value: string): value is DownloadClientKind {
  return ["qbittorrent", "rtorrent", "transmission", "deluge"].includes(value);
}

export const testConnection = async ({
  client,
  url,
  username = "",
  password,
}: {
  client: string;
  url: string;
  username: string;
  password: string;
}): Promise<{ success: boolean }> => {
  try {
    const normalizedClient = client.toLowerCase();
    if (!isDownloadClientKind(normalizedClient)) {
      return { success: false };
    }

    const result = await trpcClient.clients.testConnection.mutate({
      client: normalizedClient,
      url,
      username,
      password,
      readonly: false,
    });

    return { success: result.success };
  } catch (error) {
    console.error("Error testing connection:", error);
    return { success: false };
  }
};
