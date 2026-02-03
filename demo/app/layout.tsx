export const metadata = {
  title: 'HTTP Relay Demo',
  description: 'Demo for testing http-relay /link2 endpoint',
}

export default function RootLayout({
  children,
}: {
  children: React.ReactNode
}) {
  return (
    <html lang="en">
      <body style={{ fontFamily: 'system-ui, sans-serif', margin: 0, padding: '20px', backgroundColor: '#f5f5f5' }}>
        {children}
      </body>
    </html>
  )
}
