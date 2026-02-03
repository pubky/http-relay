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

function HomeContent() {
  const searchParams = useSearchParams()
  const [relayUrl, setRelayUrl] = useState('http://localhost:8080')
  const [endpoint, setEndpoint] = useState<'link' | 'link2'>('link2')
  const [channelId, setChannelId] = useState('')
  const [producerContent, setProducerContent] = useState('hello world')
  const [logs, setLogs] = useState<LogEntry[]>([])
  const [consumerRunning, setConsumerRunning] = useState(false)
  const [producerRunning, setProducerRunning] = useState(false)

  const consumerAbortRef = useRef<AbortController | null>(null)
  const producerAbortRef = useRef<AbortController | null>(null)

  // Read channel from URL on mount
  useEffect(() => {
    const channel = searchParams.get('channel')
    if (channel) setChannelId(channel)
  }, [searchParams])

  // Update URL when channel changes
  const updateChannelId = useCallback((newId: string) => {
    setChannelId(newId)
    const url = new URL(window.location.href)
    if (newId) {
      url.searchParams.set('channel', newId)
    } else {
      url.searchParams.delete('channel')
    }
    window.history.replaceState({}, '', url.toString())
  }, [])

  const addLog = useCallback((type: LogEntry['type'], message: string) => {
    setLogs(prev => [...prev, { timestamp: new Date(), type, message }])
  }, [])

  const startConsumer = async () => {
    if (!channelId.trim()) {
      addLog('error', 'Channel ID is required')
      return
    }

    setConsumerRunning(true)
    consumerAbortRef.current = new AbortController()
    const id = channelId.trim()

    addLog('consumer', `Starting consumer loop for ID: ${id}`)

    while (true) {
      if (consumerAbortRef.current?.signal.aborted) {
        addLog('consumer', 'Consumer stopped by user')
        break
      }

      try {
        addLog('consumer', `GET ${relayUrl}/${endpoint}/${id}`)
        const response = await fetch(`${relayUrl}/${endpoint}/${id}`, {
          signal: consumerAbortRef.current?.signal,
        })

        if (response.status === 200) {
          const data = await response.text()
          addLog('consumer', `Received: ${data}`)
          break
        }

        if (response.status === 408) {
          addLog('consumer', '408 Timeout - retrying...')
          continue
        }

        addLog('error', `Unexpected status: ${response.status}`)
        break
      } catch (err) {
        if (err instanceof Error && err.name === 'AbortError') {
          addLog('consumer', 'Consumer stopped by user')
          break
        }
        addLog('error', `Consumer error: ${err}`)
        break
      }
    }

    setConsumerRunning(false)
  }

  const stopConsumer = () => {
    consumerAbortRef.current?.abort()
  }

  const startProducer = async () => {
    if (!channelId.trim()) {
      addLog('error', 'Channel ID is required')
      return
    }

    setProducerRunning(true)
    producerAbortRef.current = new AbortController()
    const id = channelId.trim()
    const content = producerContent

    addLog('producer', `Starting producer loop for ID: ${id}`)

    while (true) {
      if (producerAbortRef.current?.signal.aborted) {
        addLog('producer', 'Producer stopped by user')
        break
      }

      try {
        addLog('producer', `POST ${relayUrl}/${endpoint}/${id} with: ${content}`)
        const response = await fetch(`${relayUrl}/${endpoint}/${id}`, {
          method: 'POST',
          headers: { 'Content-Type': 'text/plain' },
          body: content,
          signal: producerAbortRef.current?.signal,
        })

        if (response.status === 200) {
          addLog('producer', 'Consumer received the message!')
          break
        }

        if (response.status === 408) {
          addLog('producer', '408 Timeout - retrying...')
          continue
        }

        addLog('error', `Unexpected status: ${response.status}`)
        break
      } catch (err) {
        if (err instanceof Error && err.name === 'AbortError') {
          addLog('producer', 'Producer stopped by user')
          break
        }
        addLog('error', `Producer error: ${err}`)
        break
      }
    }

    setProducerRunning(false)
  }

  const stopProducer = () => {
    producerAbortRef.current?.abort()
  }

  const clearLogs = () => setLogs([])

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

  const buttonStyle = (active: boolean, color: string): React.CSSProperties => ({
    padding: '8px 16px',
    backgroundColor: active ? '#999' : color,
    color: 'white',
    border: 'none',
    borderRadius: '4px',
    cursor: active ? 'not-allowed' : 'pointer',
    fontSize: '14px',
    marginRight: '8px',
  })

  return (
    <div style={{ maxWidth: '800px', margin: '0 auto' }}>
      <h1 style={{ marginBottom: '24px' }}>HTTP Relay Demo</h1>

      {/* Config Section */}
      <div style={sectionStyle}>
        <h2 style={{ margin: '0 0 12px 0', fontSize: '18px' }}>Configuration</h2>
        <div style={{ display: 'flex', gap: '12px', marginBottom: '12px' }}>
          <div style={{ flex: 2 }}>
            <label style={{ display: 'block', marginBottom: '4px', fontSize: '12px', color: '#666' }}>Relay URL</label>
            <input
              type="text"
              value={relayUrl}
              onChange={(e) => setRelayUrl(e.target.value)}
              placeholder="http://localhost:8080"
              style={inputStyle}
            />
          </div>
          <div>
            <label style={{ display: 'block', marginBottom: '4px', fontSize: '12px', color: '#666' }}>Endpoint</label>
            <div style={{ display: 'flex' }}>
              <button
                onClick={() => setEndpoint('link2')}
                disabled={consumerRunning || producerRunning}
                style={{
                  padding: '8px 16px',
                  backgroundColor: endpoint === 'link2' ? '#2563eb' : '#e5e7eb',
                  color: endpoint === 'link2' ? 'white' : '#374151',
                  border: 'none',
                  borderRadius: '4px 0 0 4px',
                  cursor: consumerRunning || producerRunning ? 'not-allowed' : 'pointer',
                  fontSize: '14px',
                }}
              >
                /link2
              </button>
              <button
                onClick={() => setEndpoint('link')}
                disabled={consumerRunning || producerRunning}
                style={{
                  padding: '8px 16px',
                  backgroundColor: endpoint === 'link' ? '#2563eb' : '#e5e7eb',
                  color: endpoint === 'link' ? 'white' : '#374151',
                  border: 'none',
                  borderRadius: '0 4px 4px 0',
                  cursor: consumerRunning || producerRunning ? 'not-allowed' : 'pointer',
                  fontSize: '14px',
                }}
              >
                /link
              </button>
            </div>
          </div>
        </div>
        <div style={{ display: 'flex', gap: '12px' }}>
          <div style={{ flex: 1 }}>
            <label style={{ display: 'block', marginBottom: '4px', fontSize: '12px', color: '#666' }}>Channel ID</label>
            <div style={{ display: 'flex', gap: '8px' }}>
              <input
                type="text"
                value={channelId}
                onChange={(e) => updateChannelId(e.target.value)}
                placeholder="my-channel"
                style={{ ...inputStyle, flex: 1 }}
                disabled={consumerRunning || producerRunning}
              />
              <button
                onClick={() => updateChannelId(generateRandomId())}
                disabled={consumerRunning || producerRunning}
                style={{
                  padding: '8px 12px',
                  backgroundColor: consumerRunning || producerRunning ? '#ccc' : '#6b7280',
                  color: 'white',
                  border: 'none',
                  borderRadius: '4px',
                  cursor: consumerRunning || producerRunning ? 'not-allowed' : 'pointer',
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

      {/* Consumer Section */}
      <div style={sectionStyle}>
        <h2 style={{ margin: '0 0 12px 0', fontSize: '18px' }}>Consumer</h2>
        <div style={{ display: 'flex', gap: '12px', alignItems: 'center' }}>
          {!consumerRunning ? (
            <button onClick={startConsumer} style={buttonStyle(false, '#2563eb')}>
              Start Consumer
            </button>
          ) : (
            <button onClick={stopConsumer} style={buttonStyle(false, '#dc2626')}>
              Stop
            </button>
          )}
          {consumerRunning && (
            <span style={{ color: '#2563eb', fontSize: '14px' }}>
              Waiting for producer...
            </span>
          )}
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
            disabled={producerRunning}
          />
        </div>
        <div style={{ display: 'flex', gap: '12px', alignItems: 'center' }}>
          {!producerRunning ? (
            <button onClick={startProducer} style={buttonStyle(false, '#16a34a')}>
              Send
            </button>
          ) : (
            <button onClick={stopProducer} style={buttonStyle(false, '#dc2626')}>
              Stop
            </button>
          )}
          {producerRunning && (
            <span style={{ color: '#16a34a', fontSize: '14px' }}>
              Waiting for consumer...
            </span>
          )}
        </div>
      </div>

      {/* Log Section */}
      <div style={sectionStyle}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '12px' }}>
          <h2 style={{ margin: 0, fontSize: '18px' }}>Request Log</h2>
          <button onClick={clearLogs} style={{ ...buttonStyle(false, '#6b7280'), marginRight: 0 }}>
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
                      log.type === 'consumer' ? '#60a5fa' :
                      log.type === 'producer' ? '#4ade80' :
                      log.type === 'error' ? '#f87171' : '#d4d4d4',
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
