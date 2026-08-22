// Definition of the DownloadClient type
export type TDownloadClient = {
  name?: string;
  index?: number;
  client: string;
  url: string;
  user?: string;
  password: string;
  /** qBittorrent 5.2+ only: authenticate with an API key instead of a login. */
  useApiKey?: boolean;
  apiKey?: string;
  readOnly?: boolean;
};
