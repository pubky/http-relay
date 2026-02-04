'use client'

import { useState, useRef, useCallback, useEffect, Suspense } from 'react'
import { useSearchParams } from 'next/navigation'

type LogEntry = {
  timestamp: Date
  type: 'consumer' | 'producer' | 'info' | 'error'
  message: string
}

function generateRandomId() {
  return Math.random().toString(36).substring(2, 10)
}

const DEFAULT_RELAY_URL =
  process.env.NEXT_PUBLIC_RELAY_URL || 'http://localhost:8080'

function HomeContent() {
  const searchParams = useSearchParams()
  const [relayUrl, setRelayUrl] = useState(DEFAULT_RELAY_URL)
  const [channelId, setChannelId] = useState('')
  const [producerContent, setProducerContent] = useState('hello world')
  const [logs, setLogs] = useState<LogEntry[]>([])

  const [postLoading, setPostLoading] = useState(false)
  const [checkAckLoading, setCheckAckLoading] = useState(false)
  const [sendAckLoading, setSendAckLoading] = useState(false)
  const [getMessageActive, setGetMessageActive] = useState(false)
  const [awaitAckActive, setAwaitAckActive] = useState(false)

  const getMessageAbortRef = useRef<AbortController | null>(null)
  const awaitAckAbortRef = useRef<AbortController | null>(null)

  useEffect(() => {
    const channel = searchParams.get('channel')
    const relay = searchParams.get('relay')
    if (channel) setChannelId(channel)
    if (relay) setRelayUrl(relay)
  }, [searchParams])

  const updateChannelId = useCallback((newId: string) => {
    setChannelId(newId)
    setLogs([])
    const url = new URL(window.location.href)
    if (newId) {
      url.searchParams.set('channel', newId)
    } else {
      url.searchParams.delete('channel')
    }
    window.history.replaceState({}, '', url.toString())
  }, [])

  const addLog = useCallback((type: LogEntry['type'], message: string) => {
    setLogs((prev) => [...prev, { timestamp: new Date(), type, message }])
  }, [])

  const clearLogs = () => setLogs([])

  const handlePost = async () => {
    if (!channelId.trim()) {
      addLog('error', 'Channel ID is required')
      return
    }

    setPostLoading(true)
    const id = channelId.trim()

    try {
      addLog('producer', `POST /inbox/${id} with: ${producerContent}`)
      const response = await fetch(`${relayUrl}/inbox/${id}`, {
        method: 'POST',
        headers: { 'Content-Type': 'text/plain' },
        body: producerContent,
      })

      if (response.status === 200) {
        addLog('producer', 'Message stored (200)')
      } else {
        addLog('error', `Failed with status: ${response.status}`)
      }
    } catch (err) {
      addLog('error', `Error: ${err}`)
    }

    setPostLoading(false)
  }

  const handleCheckAck = async () => {
    if (!channelId.trim()) {
      addLog('error', 'Channel ID is required')
      return
    }

    setCheckAckLoading(true)
    const id = channelId.trim()

    try {
      addLog('producer', `GET /inbox/${id}/ack`)
      const response = await fetch(`${relayUrl}/inbox/${id}/ack`)

      if (response.status === 200) {
        const data = await response.text()
        addLog('producer', `ACK status: ${data}`)
      } else {
        addLog('error', `Failed with status: ${response.status}`)
      }
    } catch (err) {
      addLog('error', `Error: ${err}`)
    }

    setCheckAckLoading(false)
  }

  const handleAwaitAck = async () => {
    if (!channelId.trim()) {
      addLog('error', 'Channel ID is required')
      return
    }

    if (awaitAckActive) {
      awaitAckAbortRef.current?.abort()
      return
    }

    setAwaitAckActive(true)
    awaitAckAbortRef.current = new AbortController()
    const id = channelId.trim()

    try {
      addLog('producer', `GET /inbox/${id}/await (waiting...)`)
      const response = await fetch(`${relayUrl}/inbox/${id}/await`, {
        signal: awaitAckAbortRef.current.signal,
      })

      if (response.status === 200) {
        addLog('producer', 'ACKed!')
      } else if (response.status === 408) {
        addLog('producer', 'Timeout (408)')
      } else {
        addLog('error', `Failed with status: ${response.status}`)
      }
    } catch (err) {
      if (err instanceof Error && err.name === 'AbortError') {
        addLog('producer', 'Await ACK cancelled')
      } else {
        addLog('error', `Error: ${err}`)
      }
    }

    setAwaitAckActive(false)
  }

  const handleGetMessage = async () => {
    if (!channelId.trim()) {
      addLog('error', 'Channel ID is required')
      return
    }

    if (getMessageActive) {
      getMessageAbortRef.current?.abort()
      return
    }

    setGetMessageActive(true)
    getMessageAbortRef.current = new AbortController()
    const id = channelId.trim()

    try {
      addLog('consumer', `GET /inbox/${id} (waiting...)`)
      const response = await fetch(`${relayUrl}/inbox/${id}`, {
        signal: getMessageAbortRef.current.signal,
      })

      if (response.status === 200) {
        const data = await response.text()
        addLog('consumer', `Received: ${data}`)
      } else if (response.status === 408) {
        addLog('consumer', 'Timeout (408)')
      } else {
        addLog('error', `Failed with status: ${response.status}`)
      }
    } catch (err) {
      if (err instanceof Error && err.name === 'AbortError') {
        addLog('consumer', 'GET cancelled')
      } else {
        addLog('error', `Error: ${err}`)
      }
    }

    setGetMessageActive(false)
  }

  const handleSendAck = async () => {
    if (!channelId.trim()) {
      addLog('error', 'Channel ID is required')
      return
    }

    setSendAckLoading(true)
    const id = channelId.trim()

    try {
      addLog('consumer', `DELETE /inbox/${id}`)
      const response = await fetch(`${relayUrl}/inbox/${id}`, {
        method: 'DELETE',
      })

      if (response.status === 200) {
        addLog('consumer', 'ACK sent (200)')
      } else if (response.status === 404) {
        addLog('consumer', 'Nothing to ACK (404)')
      } else {
        addLog('error', `Failed with status: ${response.status}`)
      }
    } catch (err) {
      addLog('error', `Error: ${err}`)
    }

    setSendAckLoading(false)
  }

  const sectionStyle: React.CSSProperties = {
    backgroundColor: 'white',
    padding: '16px',
    borderRadius: '8px',
    marginBottom: '16px',
    boxShadow: '0 1px 3px rgba(0,0,0,0.1)',
  }

  const inputStyle: React.CSSProperties = {
    padding: '8px 12px',
    border: '1px solid #ddd',
    borderRadius: '4px',
    fontSize: '14px',
    width: '100%',
    boxSizing: 'border-box',
  }

  const buttonStyle = (
    color: string,
    isLoading: boolean = false
  ): React.CSSProperties => ({
    padding: '8px 16px',
    backgroundColor: color,
    color: 'white',
    border: 'none',
    borderRadius: '4px',
    cursor: 'pointer',
    fontSize: '14px',
    marginRight: '8px',
    opacity: isLoading ? 0.7 : 1,
  })

  return (
    <div style={{ maxWidth: '800px', margin: '0 auto' }}>
      <h1 style={{ marginBottom: '24px' }}>HTTP Relay Demo</h1>

      {/* Configuration Section */}
      <div style={sectionStyle}>
        <h2 style={{ margin: '0 0 12px 0', fontSize: '18px' }}>
          Configuration
        </h2>
        <div style={{ display: 'flex', gap: '12px' }}>
          <div style={{ flex: 2 }}>
            <label
              style={{
                display: 'block',
                marginBottom: '4px',
                fontSize: '12px',
                color: '#666',
              }}
            >
              Relay URL
            </label>
            <input
              type="text"
              value={relayUrl}
              onChange={(e) => setRelayUrl(e.target.value)}
              placeholder="http://localhost:8080"
              style={inputStyle}
            />
          </div>
          <div style={{ flex: 1 }}>
            <label
              style={{
                display: 'block',
                marginBottom: '4px',
                fontSize: '12px',
                color: '#666',
              }}
            >
              Channel ID
            </label>
            <div style={{ display: 'flex', gap: '8px' }}>
              <input
                type="text"
                value={channelId}
                onChange={(e) => updateChannelId(e.target.value)}
                placeholder="my-channel"
                style={{ ...inputStyle, flex: 1 }}
              />
              <button
                onClick={() => updateChannelId(generateRandomId())}
                style={{
                  padding: '8px 12px',
                  backgroundColor: '#6b7280',
                  color: 'white',
                  border: 'none',
                  borderRadius: '4px',
                  cursor: 'pointer',
                  fontSize: '12px',
                  whiteSpace: 'nowrap',
                }}
              >
                Random
              </button>
            </div>
          </div>
        </div>
      </div>

      {/* Producer Section */}
      <div style={sectionStyle}>
        <h2 style={{ margin: '0 0 12px 0', fontSize: '18px' }}>Producer</h2>
        <div style={{ marginBottom: '12px' }}>
          <textarea
            value={producerContent}
            onChange={(e) => setProducerContent(e.target.value)}
            placeholder="Message content"
            style={{ ...inputStyle, minHeight: '60px', resize: 'vertical' }}
          />
        </div>
        <div style={{ display: 'flex', alignItems: 'center' }}>
          <button
            onClick={handlePost}
            disabled={postLoading}
            style={buttonStyle('#16a34a', postLoading)}
          >
            {postLoading ? 'Posting...' : 'POST'}
          </button>
          <button
            onClick={handleCheckAck}
            disabled={checkAckLoading}
            style={buttonStyle('#6b7280', checkAckLoading)}
          >
            {checkAckLoading ? 'Checking...' : 'Check ACK'}
          </button>
          <button
            onClick={handleAwaitAck}
            style={buttonStyle(
              awaitAckActive ? '#dc2626' : '#2563eb',
              false
            )}
          >
            {awaitAckActive ? 'Cancel' : 'Await ACK'}
          </button>
          {awaitAckActive && (
            <span style={{ color: '#2563eb', fontSize: '14px' }}>
              Waiting...
            </span>
          )}
        </div>
      </div>

      {/* Consumer Section */}
      <div style={sectionStyle}>
        <h2 style={{ margin: '0 0 12px 0', fontSize: '18px' }}>Consumer</h2>
        <div style={{ display: 'flex', alignItems: 'center' }}>
          <button
            onClick={handleGetMessage}
            style={buttonStyle(
              getMessageActive ? '#dc2626' : '#2563eb',
              false
            )}
          >
            {getMessageActive ? 'Cancel' : 'GET Message'}
          </button>
          <button
            onClick={handleSendAck}
            disabled={sendAckLoading}
            style={buttonStyle('#ea580c', sendAckLoading)}
          >
            {sendAckLoading ? 'Sending...' : 'Send ACK'}
          </button>
          {getMessageActive && (
            <span style={{ color: '#2563eb', fontSize: '14px' }}>
              Waiting...
            </span>
          )}
        </div>
      </div>

      {/* Request Log Section */}
      <div style={sectionStyle}>
        <div
          style={{
            display: 'flex',
            justifyContent: 'space-between',
            alignItems: 'center',
            marginBottom: '12px',
          }}
        >
          <h2 style={{ margin: 0, fontSize: '18px' }}>Request Log</h2>
          <button
            onClick={clearLogs}
            style={{ ...buttonStyle('#6b7280'), marginRight: 0 }}
          >
            Clear
          </button>
        </div>
        <div
          style={{
            backgroundColor: '#1e1e1e',
            color: '#d4d4d4',
            padding: '12px',
            borderRadius: '4px',
            fontFamily: 'monospace',
            fontSize: '12px',
            height: '300px',
            overflowY: 'auto',
          }}
        >
          {logs.length === 0 ? (
            <div style={{ color: '#666' }}>No requests yet...</div>
          ) : (
            logs.map((log, i) => (
              <div key={i} style={{ marginBottom: '4px' }}>
                <span style={{ color: '#888' }}>
                  [{log.timestamp.toLocaleTimeString()}]
                </span>{' '}
                <span
                  style={{
                    color:
                      log.type === 'consumer'
                        ? '#60a5fa'
                        : log.type === 'producer'
                          ? '#4ade80'
                          : log.type === 'error'
                            ? '#f87171'
                            : '#d4d4d4',
                  }}
                >
                  [{log.type.toUpperCase()}]
                </span>{' '}
                {log.message}
              </div>
            ))
          )}
        </div>
      </div>
    </div>
  )
}

export default function Home() {
  return (
    <Suspense fallback={<div style={{ padding: '20px' }}>Loading...</div>}>
      <HomeContent />
    </Suspense>
  )
}
